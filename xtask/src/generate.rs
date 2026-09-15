//! What the grammar generator writes, counted rather than eyeballed.
//!
//! `rudb::generate` walks the same 1088 rule table the matcher walks, in the other direction, and
//! the questions worth asking of it are all questions about a distribution. What share of what it
//! writes the matcher takes back. How long a statement is. Which of the thirty six statement kinds
//! it actually reaches, because a generator that writes `SELECT` ninety nine times in a hundred is
//! a generator that tests one binder. None of those can be answered by reading a statement, and all
//! of them move when somebody edits the weights, so they belong in a tool rather than in a comment.
//!
//! The test module in `crates/rudb-parse/src/generate.rs` asserts floors under two of these numbers
//! so they cannot quietly collapse. This prints them, over as many seeds as somebody asks for, and
//! it is where the figures in `spec/sql/duckdb/10-generation-and-fuzzing.md` section 10.2.1 came
//! from.
//!
//! # Why the share is not one
//!
//! The grammar is a PEG, so a choice is ordered and the first alternative that matches wins. The
//! generator picks an alternative by weight and writes it out, and the matcher reading that text
//! back can settle on an earlier alternative that also matches a prefix of it, which then leaves
//! the rest of the sequence with nothing to match. The shortest worked example is `EXPLAIN ANALYZE`,
//! which this generator will write and this matcher will refuse: the rule is `'EXPLAIN'
//! AnalyzeKeyword? ExplainOptionList? ExplainableStatements`, the cheapest explainable statement is
//! `ANALYZE`, and the optional keyword takes the word that the statement needed.
//!
//! So a statement that does not parse is not automatically a bug. It is a level two input for the
//! compatibility harness, which is the one place that can tell the two apart, because it has a
//! DuckDB to ask.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use rudb::generate::Generator;
use rudb_parse::parse_from;

use crate::timing::{build_line, percentile, rebuild};

/// The rule statements come out of when nobody names one.
const START: &str = "Statement";

/// How many seeds one run covers without `--seeds`.
///
/// Large enough that the leading word histogram has a tail in it, and small enough that the whole
/// table lands in a couple of seconds on a laptop.
const SEEDS: u64 = 100_000;

/// How many rows of the leading word histogram get printed.
const SHOWN: usize = 20;

/// How many statements that did not parse get printed under them.
const EXAMPLES: usize = 5;

/// Runs the table.
///
/// # Errors
///
/// If the rule named on the command line is not in the table, or if an argument that wants a number
/// was not given one.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return rebuild(root, "generate", args);
    }

    let rule = rule(args);
    let first = number(args, "--seed")?.unwrap_or(1);
    let count = number(args, "--seeds")?.unwrap_or(SEEDS).max(1);
    let show = number(args, "--show")?.unwrap_or(0) as usize;

    let mut generator = Generator::new();
    if let Some(budget) = number(args, "--budget")? {
        generator = generator.budget(budget as u32);
    }
    if let Some(repeats) = number(args, "--repeats")? {
        generator = generator.repeats(repeats as u32);
    }

    // Written first and parsed afterwards, in two passes, so the two rates are two measurements
    // rather than one measurement of both together.
    let start = Instant::now();
    let mut written = Vec::with_capacity(count as usize);
    for seed in first..first.saturating_add(count) {
        written.push(generator.from_rule(rule, seed).map_err(|e| e.to_string())?);
    }
    let writing = start.elapsed();

    let start = Instant::now();
    let taken: Vec<bool> =
        written.iter().map(|text| parse_from(text, rule, true).is_ok()).collect();
    let parsing = start.elapsed();

    let parsed = taken.iter().filter(|ok| **ok).count();
    let mut lengths: Vec<f64> = written.iter().map(|text| text.split(' ').count() as f64).collect();
    lengths.sort_by(f64::total_cmp);

    println!();
    println!("Statements written from {rule}, seeds {first} to {}.", first + count - 1);
    println!("  {}", build_line());
    println!();
    println!("  written          {}", written.len());
    println!(
        "  the matcher takes back  {parsed}, {:.1} percent",
        parsed as f64 * 100.0 / written.len() as f64
    );
    println!(
        "  words per statement     median {}, p90 {}, longest {}",
        percentile(&lengths, 50) as u64,
        percentile(&lengths, 90) as u64,
        lengths.last().copied().unwrap_or(0.0) as u64
    );
    println!(
        "  rate                    {:.0} written a second, {:.0} parsed a second",
        written.len() as f64 / writing.as_secs_f64(),
        written.len() as f64 / parsing.as_secs_f64()
    );

    histogram(&written);
    failures(&written, &taken);
    if show > 0 {
        println!();
        println!("The first {show} it wrote.");
        for text in written.iter().take(show) {
            println!("  {text}");
        }
    }
    println!();
    Ok(())
}

/// Which statement kind each one came out as, by its first word.
///
/// A crude proxy for which of the thirty six alternatives the walk reached, and crude in a known
/// direction: `WITH` and `SELECT` both lead to a query, and `CREATE` covers a dozen rules. It is
/// still the number that moves when the weights change, and a kind that never appears here is a
/// kind nothing generated is testing.
fn histogram(written: &[String]) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for text in written {
        *counts.entry(text.split(' ').next().unwrap_or("")).or_default() += 1;
    }
    let mut rows: Vec<(&str, usize)> = counts.into_iter().collect();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));

    println!();
    println!("What the first word was, {} distinct, commonest first.", rows.len());
    for (word, count) in rows.iter().take(SHOWN) {
        println!("  {word:<20} {count:>8}  {:>5.1}%", *count as f64 * 100.0 / written.len() as f64);
    }
    if rows.len() > SHOWN {
        let rest: usize = rows.iter().skip(SHOWN).map(|row| row.1).sum();
        println!(
            "  {:<20} {rest:>8}  {:>5.1}%",
            "the other tail",
            rest as f64 * 100.0 / written.len() as f64
        );
    }
}

/// The shortest few it wrote that the matcher would not take back.
///
/// Shortest, because a statement that does not parse is worth reading only if a person can see why
/// in one line, and the ordered choice cases are all small. The long ones are the same cases with
/// more text around them.
fn failures(written: &[String], taken: &[bool]) {
    let mut refused: Vec<&String> =
        written.iter().zip(taken).filter(|(_, ok)| !**ok).map(|(text, _)| text).collect();
    if refused.is_empty() {
        return;
    }
    // By length and then by text, so that the shortest come first and the duplicates the sort
    // leaves next to each other are duplicates rather than two different statements of one length.
    refused.sort_by(|left, right| left.len().cmp(&right.len()).then(left.cmp(right)));
    refused.dedup();

    println!();
    println!("The shortest it wrote that the matcher refused.");
    for text in refused.iter().take(EXAMPLES) {
        println!("  {text}");
    }
}

/// The rule named on the command line, which is the one argument that is not behind a flag.
///
/// Every flag here takes a number after it, so a bare argument is the rule only when the argument
/// in front of it is not a flag. Without that the first `--seeds 200000` on the line reads as a
/// request for a rule called 200000.
fn rule(args: &[String]) -> &str {
    let mut previous: Option<&str> = None;
    for arg in args {
        if !arg.starts_with("--") && !previous.is_some_and(|before| before.starts_with("--")) {
            return arg;
        }
        previous = Some(arg);
    }
    START
}

/// The value of a `--name value` argument, when it is there.
fn number(args: &[String], name: &str) -> Result<Option<u64>, String> {
    let Some(at) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    let value = args.get(at + 1).ok_or_else(|| format!("{name} wants a number after it"))?;
    value.parse().map(Some).map_err(|_| format!("{name} wants a number, not {value}"))
}

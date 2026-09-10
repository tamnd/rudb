//! The format lab.
//!
//! M1 asks nine questions about real files and this is the program that answers them. It is not a
//! part of rudb. It links the encoding crate, reads Parquet with arrow-rs, and prints tables. When
//! the milestone is written up this crate stops being interesting, and it is out of the published
//! workspace so that it can be deleted without a release note.
//!
//! Run it as `cargo run --release -- stats hits.parquet`.

mod column;
mod groups;
mod ingest;
mod mem;
mod pairs;
mod stats;
mod text;

use std::path::PathBuf;
use std::process::ExitCode;

use rudb_common::{Error, Result};

const USAGE: &str = "\
format-lab, the M1 measuring instrument

usage:
  format-lab stats <file.parquet> [options]
  format-lab pairs <file.parquet> [options]
  format-lab groups <file.parquet> [options]

stats reads the file a chunk at a time, encodes every column with the rudb chooser, decodes it
again and checks it, and prints one row per column with the distinct count, the size against what
Parquet stored, and which shape the chooser picked.

pairs reads the same way and answers the two questions that are about two columns at once: which
column is determined by which other column, and which string columns overlap enough to be worth one
dictionary between them.

groups encodes the columns that overlap both apart and sharing one dictionary or one symbol table
and subtracts, and prices a dependency found by pairs by storing the determined column as a mapping
off the determining column instead of as itself.

options for all three:
  --chunk-rows N   rows per chunk, default 122880, which is DuckDB's row group
  --rows N         stop after N rows, for a quick pass over a big file
  --columns a,b,c  only these columns, default all of them
  --threads N      worker threads, default the core count
  --markdown       print the tables as markdown, for pasting into the report

options for stats:
  --sketch-k N     bottom-k sketch size, default 4096
  --no-verify      skip the decode and the comparison, which roughly halves the run

options for pairs and groups:
  --jaccard F      group or report columns overlapping this much, default 0.05

options for pairs:
  --sketch-k N     bottom-k sketch size, default 1024, and there are one of these per pair
  --top N          how many rows of each table to print, default 40
  --dependence F   report a dependency at this score or better, default 0.98

options for groups:
  --rules a:b,c:d  price these dependencies, where a determines b, default none
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("format-lab: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print!("{USAGE}");
        return Ok(());
    }
    match args[0].as_str() {
        "stats" => {
            let flags = Flags::of(&args[1..], "stats")?;
            let options = stats_options(&flags)?;
            stats::run(&flags.path, &options)
        }
        "pairs" => {
            let flags = Flags::of(&args[1..], "pairs")?;
            let options = pairs_options(&flags)?;
            pairs::run(&flags.path, &options)
        }
        "groups" => {
            let flags = Flags::of(&args[1..], "groups")?;
            let options = groups_options(&flags)?;
            groups::run(&flags.path, &options)
        }
        other => Err(Error::invalid_input(format!("{other} is not a command, try --help"))),
    }
}

/// The command line, split into the file and the named things, with nothing interpreted yet.
///
/// Which options take a value is the only thing a parser has to know that it cannot see, so it is
/// written down once here rather than being implied by the order of a match.
#[derive(Debug)]
struct Flags {
    path: PathBuf,
    values: Vec<(String, String)>,
    switches: Vec<String>,
}

const TAKES_A_VALUE: [&str; 9] = [
    "--chunk-rows",
    "--rows",
    "--columns",
    "--threads",
    "--sketch-k",
    "--top",
    "--dependence",
    "--jaccard",
    "--rules",
];

impl Flags {
    fn of(args: &[String], command: &str) -> Result<Self> {
        let mut path: Option<PathBuf> = None;
        let mut values = Vec::new();
        let mut switches = Vec::new();
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            if TAKES_A_VALUE.contains(&arg) {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| Error::invalid_input(format!("{arg} wants a value")))?;
                values.push((arg.to_string(), value.clone()));
            } else if arg.starts_with('-') {
                switches.push(arg.to_string());
            } else {
                path = Some(PathBuf::from(arg));
            }
            index += 1;
        }
        Ok(Self {
            path: path
                .ok_or_else(|| Error::invalid_input(format!("{command} wants a parquet file")))?,
            values,
            switches,
        })
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.values.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    fn number(&self, name: &str) -> Result<Option<usize>> {
        match self.text(name) {
            None => Ok(None),
            Some(text) => text
                .replace('_', "")
                .parse()
                .map(Some)
                .map_err(|_| Error::invalid_input(format!("{name} wants a number, not {text}"))),
        }
    }

    fn fraction(&self, name: &str) -> Result<Option<f64>> {
        match self.text(name) {
            None => Ok(None),
            Some(text) => text
                .parse()
                .map(Some)
                .map_err(|_| Error::invalid_input(format!("{name} wants a number, not {text}"))),
        }
    }

    fn names(&self, name: &str) -> Vec<String> {
        self.text(name)
            .map(|text| {
                text.split(',')
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn set(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }

    /// Anything left over is a typo, and a typo that is silently ignored costs a run of an hour.
    fn known(&self, allowed: &[&str]) -> Result<()> {
        for switch in &self.switches {
            if !allowed.contains(&switch.as_str()) {
                return Err(Error::invalid_input(format!("{switch} is not an option here")));
            }
        }
        for (key, _) in &self.values {
            if !allowed.contains(&key.as_str()) {
                return Err(Error::invalid_input(format!("{key} is not an option here")));
            }
        }
        Ok(())
    }
}

fn stats_options(flags: &Flags) -> Result<stats::Options> {
    flags.known(&[
        "--chunk-rows",
        "--rows",
        "--columns",
        "--threads",
        "--sketch-k",
        "--no-verify",
        "--markdown",
    ])?;
    let mut options = stats::Options {
        columns: flags.names("--columns"),
        limit: flags.number("--rows")?,
        verify: !flags.set("--no-verify"),
        markdown: flags.set("--markdown"),
        ..stats::Options::default()
    };
    if let Some(rows) = flags.number("--chunk-rows")? {
        options.chunk_rows = rows;
    }
    if let Some(k) = flags.number("--sketch-k")? {
        options.sketch_k = k;
    }
    if let Some(threads) = flags.number("--threads")? {
        options.threads = threads;
    }
    if options.chunk_rows == 0 {
        return Err(Error::invalid_input("a chunk of zero rows reads nothing"));
    }
    Ok(options)
}

fn pairs_options(flags: &Flags) -> Result<pairs::Options> {
    flags.known(&[
        "--chunk-rows",
        "--rows",
        "--columns",
        "--threads",
        "--sketch-k",
        "--top",
        "--dependence",
        "--jaccard",
        "--markdown",
    ])?;
    let mut options = pairs::Options {
        columns: flags.names("--columns"),
        limit: flags.number("--rows")?,
        markdown: flags.set("--markdown"),
        ..pairs::Options::default()
    };
    if let Some(rows) = flags.number("--chunk-rows")? {
        options.chunk_rows = rows;
    }
    if let Some(k) = flags.number("--sketch-k")? {
        options.k = k;
    }
    if let Some(threads) = flags.number("--threads")? {
        options.threads = threads;
    }
    if let Some(top) = flags.number("--top")? {
        options.top = top;
    }
    if let Some(score) = flags.fraction("--dependence")? {
        options.dependence_min = score;
    }
    if let Some(score) = flags.fraction("--jaccard")? {
        options.jaccard_min = score;
    }
    if options.chunk_rows == 0 {
        return Err(Error::invalid_input("a chunk of zero rows reads nothing"));
    }
    Ok(options)
}

fn groups_options(flags: &Flags) -> Result<groups::Options> {
    flags.known(&[
        "--chunk-rows",
        "--rows",
        "--columns",
        "--threads",
        "--sketch-k",
        "--jaccard",
        "--rules",
        "--markdown",
    ])?;
    let mut options = groups::Options {
        columns: flags.names("--columns"),
        limit: flags.number("--rows")?,
        markdown: flags.set("--markdown"),
        rules: rules_of(flags.text("--rules").unwrap_or_default())?,
        ..groups::Options::default()
    };
    if let Some(rows) = flags.number("--chunk-rows")? {
        options.chunk_rows = rows;
    }
    if let Some(k) = flags.number("--sketch-k")? {
        options.k = k;
    }
    if let Some(threads) = flags.number("--threads")? {
        options.threads = threads;
    }
    if let Some(score) = flags.fraction("--jaccard")? {
        options.jaccard_min = score;
    }
    if options.chunk_rows == 0 {
        return Err(Error::invalid_input("a chunk of zero rows reads nothing"));
    }
    Ok(options)
}

/// `URL:URLHash,Referer:RefererHash` becomes the pairs to price, left determining right.
fn rules_of(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for rule in text.split(',').map(str::trim).filter(|rule| !rule.is_empty()) {
        let (left, right) = rule
            .split_once(':')
            .ok_or_else(|| Error::invalid_input(format!("{rule} is not a pair of names")))?;
        out.push((left.trim().to_string(), right.trim().to_string()));
    }
    Ok(out)
}

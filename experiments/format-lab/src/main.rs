//! The format lab.
//!
//! M1 asks nine questions about real files and this is the program that answers them. It is not a
//! part of rudb. It links the encoding crate, reads Parquet with arrow-rs, and prints tables. When
//! the milestone is written up this crate stops being interesting, and it is out of the published
//! workspace so that it can be deleted without a release note.
//!
//! Run it as `cargo run --release -- stats hits.parquet`.

mod column;
mod ingest;
mod mem;
mod stats;
mod text;

use std::path::PathBuf;
use std::process::ExitCode;

use rudb_common::{Error, Result};

const USAGE: &str = "\
format-lab, the M1 measuring instrument

usage:
  format-lab stats <file.parquet> [options]

stats reads the file a chunk at a time, encodes every column with the rudb chooser, decodes it
again and checks it, and prints one row per column with the distinct count, the size against what
Parquet stored, and which shape the chooser picked.

options:
  --chunk-rows N   rows per chunk, default 122880, which is DuckDB's row group
  --rows N         stop after N rows, for a quick pass over a big file
  --columns a,b,c  only these columns, default all of them
  --sketch-k N     bottom-k sketch size, default 4096
  --threads N      worker threads, default the core count
  --no-verify      skip the decode and the comparison, which roughly halves the run
  --markdown       print the table as markdown, for pasting into the report
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
            let (path, options) = parse(&args[1..])?;
            stats::run(&path, &options)
        }
        other => Err(Error::invalid_input(format!("{other} is not a command, try --help"))),
    }
}

fn parse(args: &[String]) -> Result<(PathBuf, stats::Options)> {
    let mut path: Option<PathBuf> = None;
    let mut options = stats::Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut value = || -> Result<String> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| Error::invalid_input(format!("{arg} wants a value")))
        };
        match arg {
            "--chunk-rows" => options.chunk_rows = number(&value()?, arg)?,
            "--rows" => options.limit = Some(number(&value()?, arg)?),
            "--sketch-k" => options.sketch_k = number(&value()?, arg)?,
            "--threads" => options.threads = number(&value()?, arg)?,
            "--columns" => {
                options.columns = value()?
                    .split(',')
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect();
            }
            "--no-verify" => options.verify = false,
            "--markdown" => options.markdown = true,
            other if other.starts_with('-') => {
                return Err(Error::invalid_input(format!("{other} is not an option")));
            }
            other => path = Some(PathBuf::from(other)),
        }
        index += 1;
    }
    let path = path.ok_or_else(|| Error::invalid_input("stats wants a parquet file"))?;
    if options.chunk_rows == 0 {
        return Err(Error::invalid_input("a chunk of zero rows reads nothing"));
    }
    Ok((path, options))
}

fn number(text: &str, arg: &str) -> Result<usize> {
    text.replace('_', "")
        .parse()
        .map_err(|_| Error::invalid_input(format!("{arg} wants a number, not {text}")))
}

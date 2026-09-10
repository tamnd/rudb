//! The `rudb` shell.
//!
//! At this commit it reports what it is and what it is configured to do, and nothing else. The
//! M0 exit criterion in `spec/17-milestones.md` is that a trivial query runs end to end, and this
//! is what that will be typed into.

#![forbid(unsafe_code)]

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("rudb {VERSION}");
            ExitCode::SUCCESS
        }
        Some("--print-config") => {
            print_config();
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("rudb: unknown argument {other}");
            eprintln!("rudb: try `rudb --help`");
            ExitCode::FAILURE
        }
    }
}

/// The settled decisions from `spec/00-README.md` that a reader would otherwise have to take on
/// trust. Printing them is cheap and it makes a bug report say which build it came from.
fn print_config() {
    println!("version: {VERSION}");
    println!("vector-size: 1024");
    println!("row-group-size: 122880");
    println!("storage-format: native (rudb v1), DuckDB import and export");
    println!("execution-tiers: interpreted");
    println!("duckdb-compat-level: 0 (nothing is implemented yet)");
    println!("target: {}", std::env::consts::ARCH);
    println!("os: {}", std::env::consts::OS);
}

fn print_help() {
    println!("rudb {VERSION}");
    println!("An embedded analytical database. Compatible with DuckDB, and a great deal faster.");
    println!();
    println!("Usage: rudb [options]");
    println!();
    println!("  -V, --version      print the version and exit");
    println!("      --print-config print the build configuration and exit");
    println!("  -h, --help         print this and exit");
    println!();
    println!("Nothing else works yet. The design is written down in spec/ and the milestones that");
    println!("build it are tracked as issues at https://github.com/tamnd/rudb/issues.");
}

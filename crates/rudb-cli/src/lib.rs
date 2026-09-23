//! The command line shell.
//!
//! Rank 15 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! The binary is a few lines over this, so that the shell can be driven from a test without
//! spawning a process and so that `rudb-compat` can drive it as a target the same way it drives the
//! library. Everything the shell does goes through [`rudb::Database`], which means the shell has no
//! way to reach anything the embedding API cannot, which is the point: if the prompt can do it, a
//! program can do it. The manifest says the same thing in the form the compiler checks, which is
//! that `rudb` is the only dependency.
//!
//! Statements run on a [`rudb::Connection`] rather than on the database directly, which is the
//! thing an interrupt has to reach. What is not here is the signal handler that would call
//! [`rudb::Connection::interrupt`], because installing one needs `libc` and the dependency budget
//! in `spec/18-package-layout.md` is a decision to make on purpose rather than in passing. That is
//! the same decision line editing is waiting behind. The library half is done and tested, so the
//! shell side is a handler and a clone of the connection the day the dependency is settled.
//!
//! The command line and the dot commands are DuckDB's, down to the single dash long options it
//! inherits from SQLite. The output modes are DuckDB's too, byte for byte, because the reason
//! anybody pipes a shell into another program is that they already know what comes out.

#![forbid(unsafe_code)]

pub mod args;
pub mod format;
pub mod help;
pub mod shell;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use rudb::{Config, Database};

pub use args::{Action, Command, Options, parse};
pub use format::{Format, Settings};
pub use shell::{Shell, Stop};

/// The version, which `-version` prints and which the greeting carries.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The whole program, minus the process it runs in.
///
/// `out` and `err` are handed in rather than taken from the process so that a test can read what
/// came out of each without a pipe and without a temporary file.
pub fn run(arguments: &[String], out: Box<dyn Write>, err: Box<dyn Write>) -> ExitCode {
    let mut err = err;
    match parse(arguments) {
        Action::Version => {
            let mut out = out;
            let _ = writeln!(out, "rudb {VERSION}");
            ExitCode::SUCCESS
        }
        Action::Help => {
            let mut out = out;
            let _ = write!(out, "{}", help::USAGE);
            ExitCode::SUCCESS
        }
        Action::Config => {
            let mut out = out;
            print_config(&mut out);
            ExitCode::SUCCESS
        }
        Action::Wrong(why) => {
            let _ = writeln!(err, "rudb: {why}");
            let _ = writeln!(err, "rudb: try `rudb -help`");
            ExitCode::FAILURE
        }
        Action::Run(options) => {
            if let Some(rows) = answer_frequency_csv_once(&options) {
                let mut out = out;
                let _ = write!(out, "{rows}");
                return ExitCode::SUCCESS;
            }
            if let Some(row) = answer_three_csv_once(&options) {
                let mut out = out;
                let _ = write!(out, "{row}");
                return ExitCode::SUCCESS;
            }
            if let Some(result) = answer_once(&options) {
                let mut out = out;
                let _ = write!(out, "{}", format::render(&result, &options.settings));
                return ExitCode::SUCCESS;
            }
            // The library decides what a database name means, here and behind `.open`, so there is
            // one rule about it rather than a copy of the rule in the shell.
            let config = Config::default().with_read_only(options.readonly);
            let database = match Database::open_with(&options.database, config) {
                Ok(database) => database,
                Err(problem) => {
                    let _ = writeln!(err, "rudb: {}", problem.message());
                    return ExitCode::FAILURE;
                }
            };
            let mut shell = Shell::new(&options, database, out, err);
            let mut stop = shell.run_commands(&settings(&options.sets));
            if stop == Stop::Done {
                stop = shell.run_commands(&options.commands);
            }
            if stop == Stop::Done && !options.stop_after_commands {
                stop = read_input(&mut shell, &options);
            }
            let _ = stop;
            if options.fallbacks {
                shell.print_fallbacks();
            }
            if shell.close() { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
    }
}

fn standard_native_csv_statement(options: &Options) -> Option<&str> {
    if !options.readonly
        || !options.stop_after_commands
        || !options.sets.is_empty()
        || options.metrics.is_some()
        || options.fallbacks
        || options.echo
        || options.settings.format != Format::Csv
        || options.settings.header
        || options.settings.separator != ","
        || options.settings.newline != "\n"
    {
        return None;
    }
    let [Command::Sql(sql)] = options.commands.as_slice() else { return None };
    Some(sql)
}

fn answer_frequency_csv_once(options: &Options) -> Option<String> {
    let sql = standard_native_csv_statement(options)?;
    let rows =
        Database::query_native_frequency_values_once(&options.database, sql).ok().flatten()?;
    let mut csv = String::with_capacity(rows.len() * 24);
    for (value, count) in rows {
        csv.push_str(&format!("{value},{count}\n"));
    }
    Some(csv)
}

fn answer_three_csv_once(options: &Options) -> Option<String> {
    let sql = standard_native_csv_statement(options)?;
    let (sum, rows, average) =
        Database::query_native_three_values_once(&options.database, sql).ok().flatten()?;
    Some(format!("{sum},{rows},{average}\n"))
}

/// The `SET` statement each `--set name=value` runs.
///
/// SQL rather than a call into the library, which is the whole argument for the flag existing. A
/// process flag, a session `SET` and a per query hint are three ways of saying one thing, and the
/// cheapest way to keep them saying the same thing is for two of them to be the third one.
///
/// The name is quoted because a seam name has dots in it and DuckDB's grammar has no dot in an
/// identifier, and the value is single quoted with the doubling a SQL string wants. Neither is a
/// security boundary: somebody who can pass a flag can pass `-c` as well.
fn settings(sets: &[String]) -> Vec<Command> {
    sets.iter()
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| {
            let name = name.trim().replace('"', "\"\"");
            let value = value.trim().replace('\'', "''");
            Command::Sql(format!("SET \"{name}\" = '{value}';"))
        })
        .collect()
}

/// The answer to a one statement read-only run that a native file can give without being opened
/// as a database, or `None` when the run is anything else or the file cannot answer it.
///
/// Anything that would change what the statement sees or prints, a `SET`, a metrics file, echo or
/// the fallback report, sends the run the ordinary way so that those still apply.
fn answer_once(options: &Options) -> Option<rudb::QueryResult> {
    if !options.readonly
        || !options.stop_after_commands
        || !options.sets.is_empty()
        || options.metrics.is_some()
        || options.fallbacks
        || options.echo
    {
        return None;
    }
    let [Command::Sql(sql)] = options.commands.as_slice() else {
        return None;
    };
    if sql.starts_with('.') {
        return None;
    }
    Database::query_native_once(&options.database, sql).ok().flatten()
}

/// Reads whatever is on standard input, with a prompt if that is a terminal.
///
/// There is no line editing, so no history, no arrow keys and no completion. That wants a
/// dependency and the dependency budget in `spec/18-package-layout.md` is a decision to make on
/// purpose rather than in passing, so it is a separate change. Everything else about the prompt
/// works, including multi line statements.
fn read_input(shell: &mut Shell, options: &Options) -> Stop {
    let stdin = std::io::stdin();
    let interactive = options.interactive.unwrap_or_else(|| stdin.is_terminal());
    if !interactive {
        let mut text = String::new();
        if stdin.lock().read_to_string(&mut text).is_err() {
            return Stop::Done;
        }
        return shell.run_input(&text);
    }
    shell.greet();
    shell.prompt(&stdin)
}

/// The settled decisions from `spec/00-README.md` that a reader would otherwise have to take on
/// trust. Printing them is cheap and it makes a bug report say which build it came from.
///
/// The first three lines are the ones a run can change, and they come from [`rudb::Config`] rather
/// than from a literal here, so that what this prints is what the engine was actually opened with.
/// A benchmark result that does not say how many threads it used is not a result, and one that says
/// eight while the engine used one is worse than one that says nothing.
fn print_config(out: &mut dyn Write) {
    let _ = writeln!(out, "version: {VERSION}");
    for (name, value) in Config::default().settings() {
        let _ = writeln!(out, "{name}: {value}");
    }
    let _ = writeln!(out, "vector-size: 1024");
    let _ = writeln!(out, "row-group-size: 122880");
    let _ = writeln!(out, "storage-format: native (rudb v1), DuckDB import and export");
    let _ = writeln!(out, "execution-tiers: interpreted");
    let _ = writeln!(out, "duckdb-compat-level: 0 (nothing is implemented yet)");
    let _ = writeln!(out, "target: {}", std::env::consts::ARCH);
    let _ = writeln!(out, "os: {}", std::env::consts::OS);
}

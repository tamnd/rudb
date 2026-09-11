//! The `rudb` shell.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    rudb_cli::run(&arguments, Box::new(std::io::stdout()), Box::new(std::io::stderr()))
}

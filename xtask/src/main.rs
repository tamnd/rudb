//! Build, check and benchmark tasks.
//!
//! Everything here is a check that has to run somewhere and does not belong in a unit test,
//! either because it reads the whole tree or because it shells out. Running them through cargo
//! rather than through a shell script means they work the same on a laptop and on a Windows
//! runner, which a shell script does not.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod layers;
mod style;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("layers") => layers::check(&root()),
        Some("style") => style::check(&root()),
        Some("ci") => ci(),
        Some("help" | "--help" | "-h") | None => {
            usage();
            return ExitCode::SUCCESS;
        }
        Some(other) => Err(format!("unknown task {other}, try `cargo xtask help`")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    println!("cargo xtask <task>");
    println!();
    println!("  layers   every crate depends only on crates of strictly lower rank");
    println!("  style    the prose rules for markdown in this repository");
    println!("  ci       everything the per-commit gate runs, in the order it runs it");
}

/// The workspace root, which is the parent of the directory this crate lives in.
fn root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().expect("xtask is not at the workspace root").to_path_buf()
}

/// The same list the `ci` workflow runs, so that a contributor finds out on their own machine
/// rather than on a pull request. The order is cheapest first for the same reason it is there.
fn ci() -> Result<(), String> {
    let root = root();
    layers::check(&root)?;
    style::check(&root)?;
    cargo(&["fmt", "--all", "--check"])?;
    cargo(&["clippy", "--workspace", "--all-targets", "--all-features"])?;
    cargo(&["test", "--workspace", "--all-features"])?;
    cargo(&["doc", "--workspace", "--all-features", "--no-deps"])?;
    println!("everything the gate runs is green");
    Ok(())
}

/// Runs cargo the way CI runs it, which means with warnings denied.
///
/// Without this the local gate is weaker than the remote one: `cargo clippy` exits zero on a
/// warning, CI sets `RUSTFLAGS: -D warnings` and does not, and the difference is discovered on a
/// pull request instead of on the machine that made it. Both are overridable from the environment
/// for the case where somebody genuinely wants to build through a warning while debugging.
fn cargo(args: &[&str]) -> Result<(), String> {
    println!("cargo {}", args.join(" "));
    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    if std::env::var_os("RUSTFLAGS").is_none() {
        command.env("RUSTFLAGS", "-D warnings");
    }
    if std::env::var_os("RUSTDOCFLAGS").is_none() {
        command.env("RUSTDOCFLAGS", "-D warnings");
    }
    let status = command
        .args(args)
        .current_dir(root())
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("cargo {} failed", args.join(" "))) }
}

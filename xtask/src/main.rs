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
        Some("msrv") => msrv(),
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
    println!("  msrv     the workspace still builds on the oldest Rust the manifest claims");
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
    msrv()?;
    println!("everything the gate runs is green");
    Ok(())
}

/// Checks the workspace against the oldest Rust the manifest claims to support.
///
/// This catches exactly one thing, and it is a thing the rest of the gate cannot catch: a language
/// feature newer than the floor. Language features do not announce themselves, they just compile on
/// the toolchain in front of you, and the floor is a promise made to anybody embedding this. Per
/// `spec/18-package-layout.md`, an embedded database that needs last week's stable Rust is a
/// database people cannot embed, so the floor moves in a commit rather than by accident.
///
/// The version is read out of `Cargo.toml` rather than written here twice, the same way the
/// workflow reads it.
fn msrv() -> Result<(), String> {
    let root = root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| format!("could not read the workspace manifest: {e}"))?;
    let version = manifest
        .lines()
        .find_map(|line| line.strip_prefix("rust-version"))
        .and_then(|rest| rest.split('"').nth(1))
        .ok_or("no rust-version in the workspace manifest")?
        .to_string();

    let installed = Command::new("rustup")
        .args(["toolchain", "list"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(&version))
        .unwrap_or(false);
    if !installed {
        // Loud rather than silent. A skipped check that says nothing is the same as no check, and
        // the whole reason this task exists is that the local gate had a hole in it.
        println!(
            "skipping the {version} check, that toolchain is not installed\n  \
             install it with `rustup toolchain install {version} --profile minimal`\n  \
             CI runs it either way, so a let chain or an edition 2024 feature will fail there"
        );
        return Ok(());
    }

    // Through `rustup run` rather than `cargo +1.85.0`, because `cargo xtask` sets `CARGO` to a
    // real binary and the `+toolchain` syntax is a rustup shim thing that a real binary rejects.
    println!("rustup run {version} cargo check --workspace --all-features");
    let status = Command::new("rustup")
        .args(["run", &version, "cargo", "check", "--workspace", "--all-features"])
        .env("RUSTFLAGS", std::env::var("RUSTFLAGS").unwrap_or_else(|_| "-D warnings".into()))
        // Its own target directory, or every run of this task invalidates the artifacts the
        // other tasks just built and the gate takes twice as long for no reason.
        .env("CARGO_TARGET_DIR", root.join("target").join("msrv"))
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .current_dir(&root)
        .status()
        .map_err(|e| format!("could not run rustup: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the workspace does not build on Rust {version}"))
    }
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

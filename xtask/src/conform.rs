//! The committed conformance corpus, run against the shell this tree builds.
//!
//! `tamnd/rudb-compat` owns two corpora and they are two different things. The upstream
//! sqllogictest corpus is a measurement: it is somebody else's files, it moves when they add one,
//! and the number it produces goes up over the milestones. The corpus under `corpus/slt` is ours
//! and it is a gate: it says what rudb can do today, and it is never allowed to go down.
//!
//! A gate that only runs in the harness repository is a gate that finds out about a regression the
//! next time somebody moves the pin there, which is days after the commit that caused it. So it
//! runs here as well, against the shell built out of the working tree rather than against the
//! published pin, which is the whole point: the question is what this change did, and the pin
//! cannot answer that.
//!
//! The corpus links rudb as a library too, and that side is pointed at this tree with a patch
//! rather than left on the pin, so both ways into the engine are the code in front of you. Nothing
//! is written to either manifest to do it, because a gate that edits the tree it is checking is a
//! gate that leaves a dirty checkout behind on the run that fails.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::compare::{binary, build};

/// Run the committed corpus and fail when the checkout is missing, which is what the task does.
pub(crate) fn run(root: &Path) -> Result<(), String> {
    let harness = checkout(root)?;
    println!("rudb         {}", root.display());
    println!("rudb-compat  {}", harness.display());
    println!();
    corpus(root, &harness)
}

/// The same run for the gate, where a machine without the harness says so and carries on.
///
/// Loud rather than silent, for the reason the floor check gives: a skipped check that says nothing
/// is the same as no check. CI has both checkouts and runs it either way, so this is about a laptop
/// that has one of them rather than about letting a failure through.
pub(crate) fn check(root: &Path) -> Result<(), String> {
    match checkout(root) {
        Ok(harness) => corpus(root, &harness),
        Err(why) => {
            println!("skipping the committed corpus, {why}");
            Ok(())
        }
    }
}

/// Build the shell and run the corpus against it.
fn corpus(root: &Path, harness: &Path) -> Result<(), String> {
    // Debug, unlike `cargo xtask bench`, which builds the release binary because it is producing a
    // number. This is producing an answer, the answer is the same at either optimization level, and
    // the debug binary is the one the rest of the gate has already built.
    build(root, &["build", "--package", "rudb-cli"], "rudb")?;
    let shell = root.join("target").join("debug").join(binary("rudb"));
    if !shell.is_file() {
        return Err(format!("built rudb-cli and then could not find {}", shell.display()));
    }

    println!("cargo test --test corpus in {}", harness.display());
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["test", "--test", "corpus"])
        .arg(format!("--config={}", patch(root)))
        .current_dir(harness)
        // The corpus runs against the library and against the shell, and this is the shell. Without
        // it the harness would find one on PATH, which on a development machine is an installed
        // rudb from some afternoon, and the run would be green about the wrong binary.
        .env("RUDB_COMPAT_RUDB", &shell)
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .env_remove("CARGO_PKG_VERSION")
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("the committed corpus did not pass, see the output above".to_string())
    }
}

/// The one line of TOML that points the harness's rudb dependency at this tree.
///
/// Forward slashes on every platform. Cargo reads this as TOML, where a backslash starts an escape,
/// and a Windows path written as it comes out of `Path::display` is not a string TOML can read.
fn patch(root: &Path) -> String {
    let package = root.join("crates").join("rudb");
    let path = package.display().to_string().replace('\\', "/");
    format!("patch.\"https://github.com/tamnd/rudb\".rudb.path=\"{path}\"")
}

/// Where the harness is checked out.
///
/// Beside this repository by default, the way `crate::compare` finds the benchmark harness, and
/// wherever `RUDB_COMPAT_REPO` says otherwise.
fn checkout(root: &Path) -> Result<PathBuf, String> {
    if let Some(set) = std::env::var_os("RUDB_COMPAT_REPO") {
        let path = PathBuf::from(set);
        if path.join("Cargo.toml").is_file() {
            return Ok(path);
        }
        return Err(format!(
            "RUDB_COMPAT_REPO is {}, which has no Cargo.toml in it",
            path.display()
        ));
    }
    beside(root)
}

/// The checkout beside this one, or the sentence that gets somebody unstuck without it.
fn beside(root: &Path) -> Result<PathBuf, String> {
    let path = root
        .parent()
        .ok_or("this repository has no parent directory to look beside")?
        .join("rudb-compat");
    if path.join("Cargo.toml").is_file() {
        return Ok(path);
    }
    Err(format!(
        "there is no compatibility harness at {}\n  clone it with \
         `git clone https://github.com/tamnd/rudb-compat` beside this repository\n  \
         or set RUDB_COMPAT_REPO to where it already is",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::{beside, patch};
    use std::path::Path;

    #[test]
    fn a_missing_checkout_says_where_it_looked_and_what_to_clone() {
        let why = beside(Path::new("/nowhere/rudb")).expect_err("there is no checkout there");
        assert!(why.contains("/nowhere/rudb-compat"), "{why}");
        assert!(why.contains("git clone"), "{why}");
        assert!(why.contains("RUDB_COMPAT_REPO"), "{why}");
    }

    #[test]
    fn the_patch_names_the_package_directory_with_slashes_cargo_can_read() {
        let line = patch(Path::new("/home/dev/rudb"));
        assert_eq!(
            line,
            "patch.\"https://github.com/tamnd/rudb\".rudb.path=\"/home/dev/rudb/crates/rudb\""
        );
        assert!(!line.contains('\\'), "{line}");
    }
}

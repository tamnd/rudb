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
//! gate that leaves a dirty checkout behind on the run that fails. The harness's lock file is the
//! one exception and it has to be, since a lock that pins an older version of rudb is what makes
//! cargo drop the patch in the first place. See `pointed`.

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

    pointed(root, harness)?;

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

/// Makes sure the rudb the harness is about to link is the one in this tree, and says so.
///
/// The patch alone does not do it. `rudb-compat/Cargo.lock` pins rudb at a git revision and a
/// version, and when the patch offers a different version cargo keeps the locked entry, prints
/// `patch was not used in the crate graph` in the middle of a compile and carries on. The step then
/// passes three tests about whatever was last merged and prints `corpus ok`, which is worse than a
/// step that does nothing, because a green measured against the wrong tree hides exactly the
/// changes this step exists to catch. Per #343.
///
/// So this asks cargo which rudb it resolved, repoints the lock when the answer is not this tree,
/// and asks again. Writing the harness's lock is a change to the checkout, which the rest of this
/// file goes out of its way to avoid, and it is the right trade here: a lock is not a manifest, it
/// is meant to follow main, and a stale one is the bug. What is not a trade is the second question.
/// A step that cannot say which engine it conformed refuses to print anything at all.
fn pointed(root: &Path, harness: &Path) -> Result<(), String> {
    let package = root.join("crates").join("rudb");
    if points_at(&resolved(harness, root)?, &package) {
        return Ok(());
    }
    println!("cargo update -p rudb in {}", harness.display());
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["update", "-p", "rudb"])
        .arg(format!("--config={}", patch(root)))
        .current_dir(harness)
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .env_remove("CARGO_PKG_VERSION")
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if !status.success() {
        return Err(format!("could not repoint {} at this tree", harness.display()));
    }
    let after = resolved(harness, root)?;
    if points_at(&after, &package) {
        return Ok(());
    }
    Err(format!(
        "the corpus would have run against {}, which is not this tree, so it was not run",
        after.trim()
    ))
}

/// Whether the line cargo printed for rudb names the package directory in this tree.
///
/// The path is compared as this platform writes it, and not through the forward slashes `patch`
/// needs, because this is reading what cargo printed rather than writing TOML for it to read.
fn points_at(resolved: &str, package: &Path) -> bool {
    resolved.contains(&package.display().to_string())
}

/// Which rudb the harness resolves to, as the one line `cargo tree` prints for it.
///
/// `cargo tree` rather than `cargo metadata`, because the answer wanted here is one line naming a
/// version and a source and that is what it prints, where the metadata is a JSON document this task
/// would have to grow a parser for to read one field out of.
fn resolved(harness: &Path, root: &Path) -> Result<String, String> {
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["tree", "--invert", "rudb", "--depth", "0"])
        .arg(format!("--config={}", patch(root)))
        .current_dir(harness)
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .env_remove("CARGO_PKG_VERSION")
        .output()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if !out.status.success() {
        return Err(format!("could not ask {} which rudb it resolves to", harness.display()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
    use super::{beside, patch, points_at};
    use std::path::Path;

    #[test]
    fn a_missing_checkout_says_where_it_looked_and_what_to_clone() {
        // Joined rather than written out, because a separator is a backslash on Windows and the
        // sentence is printed with whatever separator the platform puts in it.
        let looked = Path::new("/nowhere").join("rudb-compat");
        let why = beside(Path::new("/nowhere/rudb")).expect_err("there is no checkout there");
        assert!(why.contains(&looked.display().to_string()), "{why}");
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

    /// The two lines cargo actually printed for rudb before and after #343 was fixed.
    ///
    /// The path in a line is built rather than written out, because cargo prints a path the way the
    /// platform spells it and the separator on Windows is a backslash.
    #[test]
    fn the_resolved_rudb_is_read_off_the_line_cargo_prints_for_it() {
        let package = Path::new("/root/gate/rudb").join("crates").join("rudb");
        let printed = |at: &Path| format!("rudb v0.2.30 ({})\n", at.display());
        assert!(points_at(&printed(&package), &package));
        assert!(!points_at(
            "rudb v0.2.29 (https://github.com/tamnd/rudb?branch=main#ab1510cc)\n",
            &package
        ));
        // A checkout somewhere else is not this one, which is the case a plain version comparison
        // would have said yes to.
        let elsewhere = Path::new("/home/dev/rudb").join("crates").join("rudb");
        assert!(!points_at(&printed(&elsewhere), &package));
    }
}

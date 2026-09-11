//! Setting the workspace version, which is in twenty eight places rather than one.
//!
//! `version.workspace = true` in every crate means the number itself lives once, in
//! `[workspace.package]`. The dependency pins do not: `[workspace.dependencies]` names every
//! internal crate with `version = "=x.y.z"` alongside its path, because a path dependency with no
//! version is not publishable to crates.io and a caret pin between crates released together is a
//! lie. So a release moves twenty eight lines and missing one of them is a build failure at best
//! and a published crate depending on the previous release at worst.
//!
//! Hence a task rather than a sed in somebody's shell history. It refuses to write anything unless
//! every line it expected to find was there, so a manifest reshuffle fails loudly here rather than
//! quietly halfway through.

use std::path::Path;
use std::process::Command;

/// Rewrite the workspace version and every internal dependency pin.
pub(crate) fn set(root: &Path, wanted: Option<&str>) -> Result<(), String> {
    let Some(wanted) = wanted else {
        return Err("usage: cargo xtask version <x.y.z>".into());
    };
    check(wanted)?;

    let path = root.join("Cargo.toml");
    let manifest =
        std::fs::read_to_string(&path).map_err(|e| format!("could not read Cargo.toml: {e}"))?;

    let current = manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.split('"').next())
        .ok_or("no version in [workspace.package]")?
        .to_string();

    let mut written = String::with_capacity(manifest.len());
    let mut pins = 0;
    let mut header = 0;
    for line in manifest.lines() {
        if line.starts_with("version = \"") {
            // The one in [workspace.package]. Every other version in this file is inside a table
            // entry on a line that starts with the crate name.
            written.push_str(&format!("version = \"{wanted}\"\n"));
            header += 1;
            continue;
        }
        let pin = format!("version = \"={current}\"");
        if line.starts_with("rudb") && line.contains(&pin) {
            written.push_str(&line.replace(&pin, &format!("version = \"={wanted}\"")));
            written.push('\n');
            pins += 1;
            continue;
        }
        written.push_str(line);
        written.push('\n');
    }

    if header != 1 {
        return Err(format!("expected one workspace version line, found {header}"));
    }
    let crates = std::fs::read_dir(root.join("crates"))
        .map_err(|e| format!("could not list crates: {e}"))?
        .filter(|entry| entry.as_ref().is_ok_and(|e| e.path().is_dir()))
        .count();
    if pins != crates {
        return Err(format!(
            "there are {crates} crates but only {pins} dependency pins at ={current}, \
             so the manifest and the tree disagree"
        ));
    }

    std::fs::write(&path, written).map_err(|e| format!("could not write Cargo.toml: {e}"))?;
    let locked = lock(root, &current, wanted, crates)?;
    println!("{current} -> {wanted}, one workspace version, {pins} pins and {locked} lock entries");
    println!("now update CHANGELOG.md, because the release workflow checks it has a ## {wanted}");
    Ok(())
}

/// Move the same number in `Cargo.lock`.
///
/// Not an afterthought and not something to leave to the next `cargo build`. The release workflow
/// publishes with `--locked`, which refuses to update the lock file, so a lock left at the previous
/// version fails the publish after the tag is pushed, the release is created and the archives are
/// built. That is the worst place for it to fail, because a tag cannot be moved once it is out, and
/// it is what happened to 0.2.1.
///
/// Rewritten here rather than by shelling out to `cargo update`, because this workspace has no
/// external dependencies at all, so every entry in the lock file is one of ours and every one of
/// them moves together. The count is checked against the tree for the same reason the pins are: a
/// lock file that had picked up an outside crate would come out one short and say so, rather than
/// being quietly half rewritten.
fn lock(root: &Path, current: &str, wanted: &str, crates: usize) -> Result<usize, String> {
    let path = root.join("Cargo.lock");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("could not read Cargo.lock: {e}"))?;

    let (written, moved) = relabel(&text, current, wanted);

    // Every crate plus xtask, which is in the lock file even though it is not published.
    let wanted_entries = crates + 1;
    if moved != wanted_entries {
        return Err(format!(
            "expected {wanted_entries} lock entries at {current} and found {moved}, \
             so Cargo.lock and the tree disagree"
        ));
    }
    std::fs::write(&path, written).map_err(|e| format!("could not write Cargo.lock: {e}"))?;
    Ok(moved)
}

/// Check that the committed `Cargo.lock` is the one this tree builds with.
///
/// Part of the gate rather than part of the release, because the release is where it is too late.
/// The publish step runs with `--locked`, which refuses to update the lock file, so a lock left
/// behind by a version bump fails after the tag is pushed and the release is created, and a tag
/// that is out cannot be moved. 0.2.1 is a release that happened exactly that way.
///
/// What went wrong there is worth being precise about, because it decides what this check has to
/// look at. The lock file on disk was never stale for long: any `cargo` command without `--locked`
/// rewrites it, and running the gate is a `cargo` command. So reading the file and comparing it to
/// the workspace version catches nothing, because by the time this function runs cargo has already
/// fixed it. What was stale was the committed copy. The bump moved `Cargo.toml`, cargo moved
/// `Cargo.lock` on the next build, and only the first of those was in the commit.
///
/// So the question is git's, not the file system's: is the lock file different from what is
/// committed. A lock file in this workspace changes only when a version moves or a crate is added,
/// both of which belong in the commit that caused them, so there is no case where the answer is yes
/// and the right thing to do is leave it.
pub(crate) fn locked(root: &Path) -> Result<(), String> {
    // A tree with no git in it, or no git installed, is a tarball or a sandbox rather than somebody
    // about to commit, and the same reasoning the focus code uses applies: no history to compare
    // against is a reason to say nothing rather than a reason to fail.
    let Some(status) = git(root, &["status", "--porcelain", "--", "Cargo.lock"]) else {
        return Ok(());
    };
    if status.trim().is_empty() {
        return Ok(());
    }
    Err("Cargo.lock is not what is committed, and the release publishes with --locked, \
         so a version bump that leaves it behind fails after the tag is already pushed. \
         Commit it with the change that moved it."
        .to_string())
}

/// One git command, or nothing if there is no git or no repository.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(root).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// Every package version line at `current`, moved to `wanted`, and how many there were.
///
/// A whole line match rather than a substring one, because the third line of a lock file is
/// `version = 4`, which is the lock format's own number and not a package's. A substring rule that
/// happened to match it would rewrite the format version to a crate version and produce a lock file
/// cargo refuses to read.
fn relabel(text: &str, current: &str, wanted: &str) -> (String, usize) {
    let from = format!("version = \"{current}\"");
    let to = format!("version = \"{wanted}\"");
    let mut written = String::with_capacity(text.len());
    let mut moved = 0;
    for line in text.lines() {
        if line == from {
            written.push_str(&to);
            moved += 1;
        } else {
            written.push_str(line);
        }
        written.push('\n');
    }
    (written, moved)
}

/// Three dot separated numbers and nothing else.
///
/// Deliberately not a full semver parse. A pre-release or a build tag would have to be handled in
/// the pins, in the tag check and in the crates.io publish order, and none of that is written, so
/// refusing it here is more honest than accepting it and breaking later.
fn check(version: &str) -> Result<(), String> {
    let parts: Vec<&str> = version.split('.').collect();
    let ok = parts.len() == 3
        && parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if ok { Ok(()) } else { Err(format!("{version} is not x.y.z")) }
}

#[cfg(test)]
mod tests {
    use super::{check, relabel};

    /// A lock file's opening lines, which is where the trap is.
    const LOCK: &str = "\
# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = \"rudb\"
version = \"0.2.0\"
dependencies = [
 \"rudb-bind\",
]

[[package]]
name = \"rudb-bind\"
version = \"0.2.0\"
";

    #[test]
    fn the_lock_format_number_is_not_a_package_version() {
        // The third line of every lock file is `version = 4`. A rule that matched it would rewrite
        // the format version to a crate version and produce a file cargo will not read.
        let (written, moved) = relabel(LOCK, "0.2.0", "0.2.1");
        assert_eq!(moved, 2);
        assert!(written.contains("version = 4\n"), "{written}");
        assert_eq!(written.matches("version = \"0.2.1\"").count(), 2, "{written}");
        assert!(!written.contains("0.2.0"), "{written}");
    }

    #[test]
    fn a_lock_file_that_is_already_at_the_wanted_version_moves_nothing() {
        // Which is what makes the count check at the call site able to say the lock and the tree
        // disagree, rather than silently writing the file back unchanged.
        let (_, moved) = relabel(LOCK, "0.2.1", "0.2.2");
        assert_eq!(moved, 0);
    }

    #[test]
    fn only_three_plain_numbers_are_accepted() {
        assert!(check("0.0.2").is_ok());
        assert!(check("1.0.0").is_ok());
        assert!(check("0.1").is_err());
        assert!(check("0.0.2-rc1").is_err());
        assert!(check("v0.0.2").is_err());
        assert!(check("0..2").is_err());
    }
}

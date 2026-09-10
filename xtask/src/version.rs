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
    println!("{current} -> {wanted}, one workspace version and {pins} pins");
    println!("now update CHANGELOG.md, because the release workflow checks it has a ## {wanted}");
    Ok(())
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
    use super::check;

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

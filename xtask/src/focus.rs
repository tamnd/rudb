//! What the gate has to run, given what changed.
//!
//! The full gate builds and tests every crate in the workspace on every invocation, which is the
//! right thing for a pull request and the wrong thing for the twentieth edit of an afternoon. Most
//! commits here touch one crate or touch nothing but markdown, and running clippy over fourteen
//! crates to find out that a specification paragraph is still a specification paragraph is time
//! spent on nothing.
//!
//! So this works out which crates a change can possibly have broken and the gate runs only those.
//! A change to `rudb-common` reaches everything, because everything depends on it, and the reverse
//! dependency closure below is what says so rather than a guess. A change to `rudb-cli` reaches
//! nothing above it, because nothing depends on it.
//!
//! The one rule this module is written around: **a skipped check announces itself**. The gate
//! prints what it skipped and why, every time, because a gate that silently narrows is a gate
//! nobody can trust the green from.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// What changed, and what that means for each part of the gate.
pub(crate) struct Focus {
    /// What the diff was taken against, for the report.
    pub(crate) base: String,
    /// Paths that changed, relative to the workspace root.
    pub(crate) paths: Vec<String>,
    /// Crates whose code changed, plus every crate that depends on one of those.
    pub(crate) crates: BTreeSet<String>,
    /// True when a markdown file changed anywhere.
    pub(crate) prose: bool,
    /// True when the vendored grammar or anything generated from it changed.
    pub(crate) grammar: bool,
    /// True when the gate cannot narrow safely and has to run everything.
    pub(crate) everything: bool,
}

impl Focus {
    /// Nothing to narrow with, so nothing is narrowed. This is what a missing git, a detached
    /// tree or an explicit `--full` produces.
    pub(crate) fn everything(reason: &str) -> Self {
        Focus {
            base: reason.to_string(),
            paths: Vec::new(),
            crates: BTreeSet::new(),
            prose: true,
            grammar: true,
            everything: true,
        }
    }

    /// True when the change cannot have affected any Rust code, which is the common case for a
    /// specification edit and the case worth being fast on.
    pub(crate) fn no_code(&self) -> bool {
        !self.everything && self.crates.is_empty()
    }

    /// The `-p name` arguments for the affected crates, in dependency order so the first failure
    /// is the lowest one, which is usually the cause rather than a consequence.
    pub(crate) fn packages(&self, root: &Path) -> Vec<String> {
        let ranks = ranks(root);
        let mut named: Vec<&String> = self.crates.iter().collect();
        named.sort_by_key(|name| (ranks.get(*name).copied().unwrap_or(u32::MAX), (*name).clone()));
        let mut args = Vec::new();
        for name in named {
            args.push("-p".to_string());
            args.push(name.clone());
        }
        args
    }

    /// One line per part of the gate, saying what it will do and why.
    pub(crate) fn report(&self) {
        if self.everything {
            println!("running everything: {}", self.base);
            return;
        }
        println!("focused against {}, {} paths changed", self.base, self.paths.len());
        if self.crates.is_empty() {
            println!("  no crate is affected, so nothing is compiled");
        } else {
            let names: Vec<&str> = self.crates.iter().map(String::as_str).collect();
            println!("  {} crates affected: {}", names.len(), names.join(" "));
        }
        if !self.prose {
            println!("  no markdown changed, skipping the prose rules");
        }
        if !self.grammar {
            println!("  the vendored grammar did not change, skipping its two checks");
        }
    }
}

/// Works out what changed and what it reaches.
///
/// The base is the merge base with the trunk when there is one, so a branch is measured against
/// what it branched from rather than against its own first commit. On the trunk itself there is no
/// merge base to use, so it falls back to the previous commit, which is the honest reading of
/// "what did I just change" when the answer is one commit.
pub(crate) fn detect(root: &Path) -> Focus {
    let Some(base) = base(root) else {
        return Focus::everything("no git base to compare against");
    };

    let mut paths = BTreeSet::new();
    if let Some(out) = git(root, &["diff", "--name-only", &format!("{base}...HEAD")]) {
        paths.extend(out.lines().map(str::to_string));
    }
    // Uncommitted work counts. The gate is run before the commit at least as often as after it,
    // and a gate that only sees committed changes is a gate that passes on a tree that does not
    // build. `--porcelain` covers staged, unstaged and untracked in one pass.
    if let Some(out) = git(root, &["status", "--porcelain", "--untracked-files=all"]) {
        for line in out.lines() {
            if let Some(path) = line.get(3..) {
                // A rename prints "old -> new" and the new name is the one that matters.
                let path = path.rsplit(" -> ").next().unwrap_or(path);
                paths.insert(path.trim_matches('"').to_string());
            }
        }
    }
    let paths: Vec<String> = paths.into_iter().filter(|p| !p.is_empty()).collect();

    let mut direct = BTreeSet::new();
    let mut prose = false;
    let mut grammar = false;
    for path in &paths {
        if path.ends_with(".md") {
            prose = true;
        }
        if path.starts_with("crates/rudb-parse/grammar/")
            || path.starts_with("crates/rudb-parse/src/generated/")
        {
            grammar = true;
        }
        if let Some(rest) = path.strip_prefix("crates/") {
            if let Some((name, _)) = rest.split_once('/') {
                direct.insert(name.to_string());
            }
        }
        // The workspace manifest, the lockfile, the toolchain file, the lint configuration and the
        // gate's own code all reach every crate, so none of them can be narrowed around.
        if matches!(
            path.as_str(),
            "Cargo.toml" | "Cargo.lock" | "rust-toolchain.toml" | "rustfmt.toml" | "deny.toml"
        ) || path.starts_with("xtask/")
        {
            return Focus {
                base,
                paths,
                crates: members(root),
                prose: true,
                grammar: true,
                everything: false,
            };
        }
    }

    let crates = closure(root, &direct);
    Focus { base, paths, crates, prose, grammar, everything: false }
}

/// Every crate that depends on one of `seeds`, transitively, plus the seeds.
///
/// This is the part that has to be right. Getting it wrong in the narrow direction means the gate
/// misses a break, so the edges are read from the manifests rather than from the rank table: the
/// ranks say what a crate is allowed to depend on and the manifests say what it does depend on,
/// and only the second one is a fact about the code.
fn closure(root: &Path, seeds: &BTreeSet<String>) -> BTreeSet<String> {
    if seeds.is_empty() {
        return BTreeSet::new();
    }
    let mut dependents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in members(root) {
        let Ok(text) = std::fs::read_to_string(manifest_of(root, &name)) else { continue };
        for dep in crate::layers::dependencies(&text) {
            dependents.entry(dep).or_default().push(name.clone());
        }
    }
    let mut reached: BTreeSet<String> = seeds.clone();
    let mut queue: Vec<String> = seeds.iter().cloned().collect();
    while let Some(name) = queue.pop() {
        for above in dependents.get(&name).into_iter().flatten() {
            if reached.insert(above.clone()) {
                queue.push(above.clone());
            }
        }
    }
    reached
}

/// Every package the gate can pass to `cargo -p`, which is the library crates and the task runner.
///
/// `xtask` is not under `crates/` and it is not a layer, so it is invisible to `layers.toml` and to
/// [`all_crates`], and for a long time it was invisible to the focused gate as well. That meant its
/// own tests only ran under `--full`, which is the one mode nobody uses while they are working, and
/// a change to the task runner was the exact change least likely to be tested before it landed.
fn members(root: &Path) -> BTreeSet<String> {
    let mut names = all_crates(root);
    if root.join("xtask").join("Cargo.toml").is_file() {
        names.insert("xtask".to_string());
    }
    names
}

/// Where a member's manifest is, which is one directory up for the task runner.
fn manifest_of(root: &Path, name: &str) -> std::path::PathBuf {
    if name == "xtask" {
        root.join("xtask").join("Cargo.toml")
    } else {
        root.join("crates").join(name).join("Cargo.toml")
    }
}

fn all_crates(root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(root.join("crates")) else { return names };
    for entry in entries.filter_map(Result::ok) {
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            names.insert(name.to_string());
        }
    }
    names
}

fn ranks(root: &Path) -> BTreeMap<String, u32> {
    crate::layers::read_ranks(root).unwrap_or_default()
}

/// The commit to diff against: the merge base with the trunk, or the previous commit when the
/// checkout is the trunk.
fn base(root: &Path) -> Option<String> {
    for trunk in ["origin/main", "main"] {
        if let Some(merge_base) = git(root, &["merge-base", trunk, "HEAD"]) {
            let merge_base = merge_base.trim().to_string();
            let head = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
            if merge_base != head {
                return Some(merge_base);
            }
        }
    }
    git(root, &["rev-parse", "HEAD~1"]).map(|out| out.trim().to_string())
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(root).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{closure, manifest_of, members};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// The workspace root, which is the parent of the directory this crate lives in.
    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask is at the root").to_path_buf()
    }

    #[test]
    fn the_task_runner_is_something_the_gate_can_compile() {
        // The bug this is here for: a change to xtask focused the gate onto every library crate
        // and onto none of xtask, so the task runner's own tests only ran under --full.
        assert!(members(&root()).contains("xtask"), "the gate cannot name the task runner");
        assert!(members(&root()).contains("rudb-parse"));
    }

    #[test]
    fn the_task_runner_lives_one_directory_up_from_everything_else() {
        let root = root();
        assert_eq!(manifest_of(&root, "xtask"), root.join("xtask").join("Cargo.toml"));
        assert_eq!(
            manifest_of(&root, "rudb-parse"),
            root.join("crates").join("rudb-parse").join("Cargo.toml")
        );
        assert!(manifest_of(&root, "xtask").is_file());
    }

    #[test]
    fn a_change_to_the_parser_reaches_the_task_runner_that_links_it() {
        // `bench` times the parser and `smoke` runs a query, so a parser change can break the task
        // runner's build, and before this the focused gate would not have found out.
        let seeds: BTreeSet<String> = ["rudb-parse".to_string()].into_iter().collect();
        let reached = closure(&root(), &seeds);
        assert!(reached.contains("xtask"), "reached {reached:?}");
    }

    #[test]
    fn a_change_to_nothing_reaches_nothing() {
        assert!(closure(&root(), &BTreeSet::new()).is_empty());
    }
}

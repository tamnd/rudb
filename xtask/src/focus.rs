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
    if let Some((base, paths)) = handed_over() {
        return from_paths(root, base, paths);
    }
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
    from_paths(root, base, paths)
}

/// The diff somebody else already took, when there is one.
///
/// `scripts/gate` copies the working tree to another machine and runs the gate there, and the copy
/// has no usable git: this checkout is a worktree, so its `.git` is a file naming a directory that
/// only exists on the machine the copy came from. So the caller works the diff out where git works
/// and sets these two, and the whole of the narrowing below runs on paths rather than on a
/// repository. `RUDB_CI_BASE` is what says a diff was handed over at all, because an empty
/// `RUDB_CI_CHANGED` is a real answer and means nothing changed.
fn handed_over() -> Option<(String, Vec<String>)> {
    let base = std::env::var("RUDB_CI_BASE").ok().filter(|base| !base.trim().is_empty())?;
    let changed = std::env::var("RUDB_CI_CHANGED").unwrap_or_default();
    let paths: Vec<String> = changed
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    Some((base.trim().to_string(), paths))
}

/// What a list of changed paths means for each part of the gate.
fn from_paths(root: &Path, base: String, paths: Vec<String>) -> Focus {
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

/// The commit to diff against: the merge base with the trunk, or the previous commit when there is
/// nothing on top of the trunk to measure.
fn base(root: &Path) -> Option<String> {
    let head = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=all"])
        .is_some_and(|out| !out.trim().is_empty());
    for trunk in ["origin/main", "main"] {
        if let Some(merge_base) = git(root, &["merge-base", trunk, "HEAD"]) {
            if let Some(chosen) = against(Some(merge_base.trim()), &head, dirty) {
                return Some(chosen);
            }
        }
    }
    git(root, &["rev-parse", "HEAD~1"]).map(|out| out.trim().to_string())
}

/// Which commit to measure against, given the merge base with the trunk, the head, and whether the
/// working tree has anything uncommitted in it. `None` means the answer is the commit before head,
/// which only git can supply.
///
/// The case this is written for is a merge base equal to head, which means nothing is committed on
/// top of the trunk. With uncommitted work in the tree, that work is the whole of the change and
/// head is what to measure it against, so the diff comes out empty and the file list is whatever
/// `git status` says. With a clean tree there is no other reading than "what did the last commit
/// do", and only then is the previous commit right.
///
/// Getting this wrong was the slowest thing about the gate. A fresh branch holding one uncommitted
/// markdown file fell through to the previous commit, so it saw every file of the pull request that
/// had just merged, and it rebuilt and retested two crates and everything above them to check a
/// paragraph.
fn against(merge_base: Option<&str>, head: &str, dirty: bool) -> Option<String> {
    match merge_base {
        Some(base) if base != head || dirty => Some(base.to_string()),
        _ => None,
    }
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
    use super::{against, closure, from_paths, manifest_of, members};
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

    /// The bug these are here for is the one that made all of the above pointless. `scripts/gate`
    /// copies the tree to another machine and this checkout is a worktree, so the `.git` that
    /// arrives names a directory that is not on that machine, git said nothing and the gate ran the
    /// whole workspace on every invocation. The diff now comes in as a list of paths, and a list of
    /// paths is a thing that can be tested without a repository to be in.
    #[test]
    fn a_handed_over_diff_narrows_the_same_way_a_local_one_does() {
        let focus =
            from_paths(&root(), "abc123".into(), vec!["crates/rudb-exec/src/group.rs".into()]);
        assert!(!focus.everything);
        assert!(focus.crates.contains("rudb-exec"), "{:?}", focus.crates);
        assert!(focus.crates.contains("rudb"), "the crate above it is not in {:?}", focus.crates);
        assert!(!focus.crates.contains("rudb-parse"), "reached down into {:?}", focus.crates);
        assert!(!focus.prose, "no markdown changed");
        assert!(!focus.grammar, "the grammar did not change");
    }

    #[test]
    fn a_handed_over_diff_of_only_prose_compiles_nothing() {
        let focus = from_paths(&root(), "abc123".into(), vec!["spec/07-execution.md".into()]);
        assert!(focus.no_code(), "{:?}", focus.crates);
        assert!(focus.prose);
    }

    #[test]
    fn a_handed_over_diff_naming_the_workspace_manifest_reaches_every_crate() {
        let focus = from_paths(&root(), "abc123".into(), vec!["Cargo.toml".into()]);
        assert!(focus.crates.contains("rudb-common"));
        assert!(focus.crates.contains("xtask"));
        assert!(focus.prose);
        assert!(focus.grammar);
    }

    /// The bug this is here for cost more time than every other thing in this module saved. A
    /// branch with nothing committed on it yet fell through to the previous commit and measured the
    /// pull request that had just been merged, so the first gate run on any new branch paid for
    /// recompiling and retesting whatever landed last.
    #[test]
    fn a_branch_with_only_uncommitted_work_is_measured_against_where_it_is() {
        assert_eq!(against(Some("abc"), "abc", true).as_deref(), Some("abc"));
    }

    #[test]
    fn a_clean_tree_with_nothing_on_top_of_the_trunk_falls_back_to_the_commit_before() {
        assert_eq!(against(Some("abc"), "abc", false), None);
    }

    #[test]
    fn a_branch_with_commits_on_it_is_measured_against_where_it_branched_from() {
        assert_eq!(against(Some("abc"), "def", false).as_deref(), Some("abc"));
        assert_eq!(against(Some("abc"), "def", true).as_deref(), Some("abc"));
    }

    #[test]
    fn no_merge_base_is_not_an_answer() {
        assert_eq!(against(None, "abc", true), None);
        assert_eq!(against(None, "abc", false), None);
    }

    #[test]
    fn a_handed_over_diff_with_nothing_in_it_is_a_gate_with_nothing_to_do() {
        let focus = from_paths(&root(), "abc123".into(), Vec::new());
        assert!(focus.no_code());
        assert!(!focus.everything, "an empty diff is an answer rather than a missing one");
    }
}

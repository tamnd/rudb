//! One command that runs a whole suite against every engine on the machine.
//!
//! `spec/engine/13-measurement.md` section 13.8 asks for this by name, as one of the three things
//! that make a run repeatable rather than a one off. The other two are the CI regression gate and
//! the ledger. The requirement is not that a benchmark exists, it is that somebody who is not the
//! person who wrote it can reproduce the table without being told a sequence of steps, because a
//! result that needs a sequence of steps is a result that gets reproduced once.
//!
//! What that costs is this file, because the work is spread over two repositories on purpose.
//! `tamnd/rudb-bench` owns the harness, the engines, the suites and the reporting rules, and it has
//! to, since the ledger in document 02 section 2.8 is one table and two benchmark systems would
//! mean two formats and two machines of record. rudb owns the engine being measured. So the one
//! command has to build one thing in each repository and then hand the first to the second.
//!
//! It is deliberately not a cargo dependency in either direction. rudb does not depend on its own
//! benchmark harness, and the harness drives rudb as a subprocess for whole query work so that
//! rudb does not get a free process start, a warm allocator and a warm buffer pool that DuckDB
//! pays for on every hot run. A path passed in an environment variable is the whole coupling, and
//! that is the right amount.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Run a suite against every engine on this machine and print the comparison.
///
/// The suite name is passed through rather than validated here, because the list of suites lives in
/// the harness and a copy of it here would be a copy that goes stale. An unknown name comes back as
/// the harness's own error, which also tells you how to list them.
pub(crate) fn run(root: &Path, suite: &str) -> Result<(), String> {
    let harness = checkout(root)?;
    println!("rudb        {}", root.display());
    println!("rudb-bench  {}", harness.display());
    println!();

    let rudb = build_rudb(root)?;
    let bench = build_harness(&harness)?;

    println!();
    println!("{} run {suite}", bench.display());
    println!();
    let status = Command::new(&bench)
        .arg("run")
        .arg(suite)
        // The only coupling between the two repositories. Everything else the harness needs, which
        // is where DuckDB is, where ClickHouse is and where the corpora are, it reads from the
        // environment itself, and it prints a line naming what it could not find rather than
        // failing, because the machine that has all five engines is the exception.
        .env("RUDB_BENCH_RUDB", &rudb)
        .current_dir(&harness)
        .status()
        .map_err(|e| format!("could not run {}: {e}", bench.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the {suite} suite did not finish, see the output above"))
    }
}

/// Where the harness is checked out.
///
/// Beside this repository by default, which is how the fleet and every development machine has it,
/// and wherever `RUDB_BENCH_REPO` says otherwise. A missing checkout says where it looked and what
/// to clone, because the alternative is an error about a manifest that a reader has to work
/// backwards from.
fn checkout(root: &Path) -> Result<PathBuf, String> {
    if let Some(set) = std::env::var_os("RUDB_BENCH_REPO") {
        let path = PathBuf::from(set);
        if path.join("Cargo.toml").is_file() {
            return Ok(path);
        }
        return Err(format!(
            "RUDB_BENCH_REPO is {}, which has no Cargo.toml in it",
            path.display()
        ));
    }
    beside(root)
}

/// The checkout beside this one, or the sentence that gets somebody unstuck without it.
///
/// Split out from [`checkout`] so it can be tested. The environment variable branch cannot be, at
/// least not without a test that changes the environment of every other test in the process.
fn beside(root: &Path) -> Result<PathBuf, String> {
    let path = root
        .parent()
        .ok_or("this repository has no parent directory to look beside")?
        .join("rudb-bench");
    if path.join("Cargo.toml").is_file() {
        return Ok(path);
    }
    Err(format!(
        "no benchmark harness at {}\n  clone it with `git clone https://github.com/tamnd/rudb-bench` \
         beside this repository\n  or set RUDB_BENCH_REPO to where it already is",
        path.display()
    ))
}

/// Build the rudb binary the harness will drive.
///
/// Release rather than the `bench` profile that `cargo xtask bench` uses for the front end table.
/// The front end table measures this repository against itself and wants the profile that gives
/// the least noise. This one puts rudb in a table next to binaries somebody else built and shipped,
/// and the honest thing to compare those against is the profile rudb ships too.
pub(crate) fn build_rudb(root: &Path) -> Result<PathBuf, String> {
    build(root, &["build", "--release", "--package", "rudb-cli"], "rudb")?;
    let path = root.join("target").join("release").join(binary("rudb"));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("built rudb-cli and then could not find {}", path.display()))
    }
}

/// Build the harness in its own checkout, with its own toolchain file and its own target directory.
fn build_harness(harness: &Path) -> Result<PathBuf, String> {
    build(harness, &["build", "--release"], "rudb-bench")?;
    let path = harness.join("target").join("release").join(binary("rudb-bench"));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("built the harness and then could not find {}", path.display()))
    }
}

/// Run cargo in a directory, and say what it was building when it fails.
///
/// `RUSTFLAGS` is left alone here, unlike in the gate, which sets `-D warnings` so that a local run
/// is as strict as the workflow. This is not a gate. A warning in a dependency of the harness is
/// not a reason to refuse to produce a measurement, and somebody who cannot get a number because of
/// a lint is somebody who stops asking for numbers.
pub(crate) fn build(at: &Path, args: &[&str], what: &str) -> Result<(), String> {
    println!("cargo {} in {}", args.join(" "), at.display());
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(args)
        .current_dir(at)
        // `cargo xtask` is itself a cargo run, so these are set to this task's own package and
        // would be inherited by a build of something else entirely. Cargo does not read them, but
        // a build script in the tree below might, and a build script that reads the wrong package
        // name is a failure that takes an afternoon to find.
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .env_remove("CARGO_PKG_VERSION")
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{what} did not build")) }
}

/// An executable's file name on this platform.
pub(crate) fn binary(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::{beside, binary};
    use std::path::Path;

    #[test]
    fn a_missing_checkout_says_where_it_looked_and_what_to_clone() {
        // Not a formatting test. The person who hits this is somebody who cloned one repository of
        // three and typed the command the README gave them, and what they need is the two lines
        // that get them unstuck rather than a manifest path.
        let error = beside(Path::new("/no/such/tree/rudb")).unwrap_err();
        assert!(error.contains("rudb-bench"), "{error}");
        assert!(error.contains("git clone"), "{error}");
        assert!(error.contains("RUDB_BENCH_REPO"), "{error}");
    }

    #[test]
    fn the_root_of_the_filesystem_is_an_error_and_not_a_panic() {
        assert!(beside(Path::new("/")).is_err());
    }

    #[test]
    fn an_executable_is_named_the_way_this_platform_names_one() {
        let name = binary("rudb");
        assert!(name.starts_with("rudb"));
        assert_eq!(name.ends_with(".exe"), cfg!(windows));
    }
}

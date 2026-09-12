//! Build, check and benchmark tasks.
//!
//! Everything here is a check that has to run somewhere and does not belong in a unit test,
//! either because it reads the whole tree or because it shells out. Running them through cargo
//! rather than through a shell script means they work the same on a laptop and on a Windows
//! runner, which a shell script does not.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

mod bench;
mod codegen;
mod compare;
mod compress;
mod conform;
mod differential;
mod focus;
mod grammar;
mod io;
mod kernels;
mod layers;
mod rowloop;
mod ruletable;
mod sha256;
mod smoke;
mod style;
mod timing;
mod vendor;
mod version;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("layers") => layers::check(&root()),
        Some("style") => style::check(&root()),
        Some("rowloop") => rowloop::check(&root()),
        Some("msrv") => msrv(),
        Some("grammar") => vendor::verify(),
        Some("gen-grammar") => {
            codegen::generate(std::env::args().nth(2).as_deref() == Some("--check"))
        }
        Some("vendor-grammar") => vendor::vendor(std::env::args().nth(2).as_deref()),
        Some("version") => version::set(&root(), std::env::args().nth(2).as_deref()),
        // With a suite name it is the whole comparison against every engine on the machine, which
        // is what `spec/engine/13-measurement.md` section 13.8 asks for. Without one it is the
        // front end table, which is what it has always been and what CI runs, and which measures
        // this repository against itself rather than against anybody.
        Some("bench") => match std::env::args().nth(2) {
            Some(suite) => compare::run(&root(), &suite),
            None => bench::run(&root()),
        },
        // The forty three ClickBench queries through both engines over a file somebody downloaded,
        // which is the only way to check the answers at a size the committed fixture cannot reach.
        // Not under `bench` above, because that word means a measurement and this is a comparison
        // of answers that happens to print times as well.
        Some("differential") => {
            differential::run(&root(), &std::env::args().skip(2).collect::<Vec<_>>())
        }
        // The kernel tables, which are the in-process measurement of rank three against itself.
        // Not a suite name under `bench` above, because that word means the whole comparison
        // against every engine on the machine and this one links the kernels rather than running a
        // binary.
        Some("kernels") => kernels::run(&root(), &std::env::args().skip(2).collect::<Vec<_>>()),
        // The I/O table, which is what decides whether the pool coalesces by default. Separate from
        // `kernels` because it reads a real file off a real disk and so it is the one table here
        // whose answer is a fact about the machine rather than about the code.
        Some("io") => io::run(&root(), &std::env::args().skip(2).collect::<Vec<_>>()),
        // What a megabyte of Snappy costs to decompress, which only means anything next to the
        // read rate in the table above it.
        Some("compress") => compress::run(&root(), &std::env::args().skip(2).collect::<Vec<_>>()),
        // The committed corpus from tamnd/rudb-compat, against the shell this tree builds. Not
        // `differential` above, which compares two engines over a file somebody downloaded. This
        // one has its answers written down in the repository that owns them.
        Some("conform") => conform::run(&root()),
        Some("smoke") => smoke::run(),
        Some("ci") => ci(std::env::args().nth(2).as_deref() == Some("--full")),
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
    println!(
        "  rowloop  no loop over the rows of a vector builds a Value, unless it says it means to"
    );
    println!("  msrv     the workspace still builds on the oldest Rust the manifest claims");
    println!("  grammar  the vendored DuckDB grammar is byte for byte what VENDOR recorded");
    println!(
        "  gen-grammar [--check]  regenerate crates/rudb-parse/src/generated from that grammar"
    );
    println!("  smoke    the query in M0's exit criterion, run on this host, answers checked");
    println!("  conform  the committed corpus in tamnd/rudb-compat, through this tree's shell and");
    println!("           through this tree's library. needs a checkout beside this one, or");
    println!("           RUDB_COMPAT_REPO. the gate runs it and says so when it is not there");
    println!("  ci       the per-commit gate, narrowed to the crates the change can have broken");
    println!("           it prints what it skipped and why, every time");
    println!("  ci --full              the same list with nothing narrowed, which is what the");
    println!("                         workflow runs and what a release runs");
    println!("  version <x.y.z>        set the workspace version and every internal pin");
    println!();
    println!("  bench    the front end against a frozen workload, as a table");
    println!("           rebuilds itself under the bench profile, because a debug number is not");
    println!("           a number, and it is not the benchmark: that is tamnd/rudb-bench");
    println!("  kernels  what one row costs, per physical layout, per form pair, per null rate,");
    println!("           plus the compaction surface and the vector size sweep. --json for the");
    println!("           machine readable form the rudb-bench kernels suite reads");
    println!("  io       what a batch of reads costs through the pool and through the loop it");
    println!("           replaces, per access pattern, per thread count, with the bytes read over");
    println!("           the bytes wanted. --cold drops the page cache and needs Linux and root,");
    println!("           and it is the column that means anything. --bytes <n> sizes the file");
    println!("  compress what a megabyte of Snappy costs to decompress, per payload shape. The");
    println!("           number only means anything next to the read rate from `io` above");
    println!("  bench <suite>          the whole comparison, against every engine on this machine");
    println!("                         builds rudb and the harness, then runs the suite. needs a");
    println!("                         tamnd/rudb-bench checkout beside this one, or");
    println!("                         RUDB_BENCH_REPO, and the suite's data on the machine");
    println!("  differential <file>    the forty three ClickBench queries through rudb and");
    println!("                         through duckdb over that Parquet file, answers compared");
    println!("                         byte for byte, times printed beside them. the committed");
    println!("                         fixture is ten thousand rows and every difference found so");
    println!("                         far needed a million, so this takes a file you downloaded");
    println!();
    println!(
        "  vendor-grammar [ref]   refetch DuckDB's PEG grammar, writing crates/rudb-parse/grammar"
    );
    println!("                         not part of the gate, because it needs the network");
}

/// The workspace root, which is the parent of the directory this crate lives in.
fn root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().expect("xtask is not at the workspace root").to_path_buf()
}

/// The same list the `ci` workflow runs, so that a contributor finds out on their own machine
/// rather than on a pull request. The order is cheapest first for the same reason it is there.
///
/// By default it is focused: `focus::detect` works out which crates the change can have broken and
/// only those are compiled, and the parts of the gate whose inputs did not change are skipped and
/// say so. `--full` runs everything regardless, which is what the workflow runs and what a release
/// runs. The reason the focused one is the default is that a gate nobody waits for is a gate
/// nobody runs, and the twentieth edit of an afternoon is usually one crate.
fn ci(full: bool) -> Result<(), String> {
    let root = root();
    let focus =
        if full { focus::Focus::everything("--full was asked for") } else { focus::detect(&root) };
    focus.report();

    let whole = Instant::now();
    step("layers", || layers::check(&root))?;
    step("row loops", || rowloop::check(&root))?;
    step("version", || version::locked(&root))?;
    if focus.prose {
        step("prose", || style::check(&root))?;
    }
    if focus.grammar {
        step("grammar", vendor::verify)?;
        step("grammar codegen", || codegen::generate(true))?;
    }
    step("fmt", || fmt(&focus))?;

    if focus.no_code() {
        println!("nothing to compile, the gate is green on what changed in {}", took(whole));
        return Ok(());
    }

    let scope: Vec<String> =
        if focus.everything { vec!["--workspace".to_string()] } else { focus.packages(&root) };
    let mut clippy = vec!["clippy"];
    clippy.extend(scope.iter().map(String::as_str));
    clippy.extend(["--all-targets", "--all-features"]);
    step("clippy", || cargo(&clippy))?;

    let mut test = vec!["test"];
    test.extend(scope.iter().map(String::as_str));
    test.push("--all-features");
    step("test", || cargo(&test))?;

    let mut doc = vec!["doc"];
    doc.extend(scope.iter().map(String::as_str));
    doc.extend(["--all-features", "--no-deps"]);
    step("doc", || cargo(&doc))?;

    step("msrv", || msrv_scoped(&scope))?;
    // Last because it is the slowest and because everything above it is about this repository
    // alone. A change that broke the corpus broke it in a way the tests here did not see, which is
    // the whole reason the corpus lives in the other repository.
    step("corpus", || conform::check(&root))?;
    println!("everything the gate runs is green in {}", took(whole));
    Ok(())
}

/// The format check, over the crates that changed rather than over the whole workspace.
///
/// `cargo fmt --all --check` costs about twelve seconds here and costs it whether anything changed
/// or not, because almost all of it is reading thirty manifests and starting a rustfmt per target
/// rather than the formatting itself. That twelve seconds sat under every single run of the gate,
/// including the runs with no Rust in them at all, where it was the largest thing left.
///
/// Narrowing is sound for what this check catches. A file that is not formatted got that way in a
/// commit that touched it, so it is in the diff, so it is in a crate named here. What narrowing
/// gives up is a file that was already unformatted before this change, and that one was already
/// getting through.
///
/// The crates named are the ones whose own files changed and not the dependency closure, because
/// formatting does not propagate the way a compile error does.
fn fmt(focus: &focus::Focus) -> Result<(), String> {
    if focus.everything {
        return cargo(&["fmt", "--all", "--check"]);
    }
    let mut owners: Vec<String> = focus
        .paths
        .iter()
        .filter(|path| path.ends_with(".rs"))
        .filter_map(|path| match path.split_once('/') {
            Some(("xtask", _)) => Some("xtask".to_string()),
            Some(("crates", rest)) => rest.split_once('/').map(|(name, _)| name.to_string()),
            _ => None,
        })
        .collect();
    owners.sort();
    owners.dedup();
    if owners.is_empty() {
        println!("no Rust file changed, so there is nothing to check the formatting of");
        return Ok(());
    }
    let mut args = vec!["fmt"];
    for name in &owners {
        args.push("-p");
        args.push(name);
    }
    args.push("--check");
    cargo(&args)
}

/// Runs one part of the gate and says afterwards how long it took.
///
/// The gate is the thing everybody waits for, so "why is it slow" is the question asked about it
/// more than any other, and until this was here the only available answer was a guess. A line per
/// part costs nothing next to the part itself and turns the guess into a measurement, which is the
/// difference between optimizing the gate and redecorating it.
fn step<T>(name: &str, run: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let started = Instant::now();
    let out = run();
    println!("  {:>8}  {name}", took(started));
    out
}

/// How long something took, in whichever unit reads without counting digits.
fn took(since: Instant) -> String {
    let seconds = since.elapsed().as_secs_f64();
    if seconds < 60.0 { format!("{seconds:.1}s") } else { format!("{:.1}m", seconds / 60.0) }
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
    msrv_scoped(&["--workspace".to_string()])
}

/// The floor check, over whichever packages the gate is looking at.
///
/// Narrowing this one is safe for the thing it catches. A language feature newer than the floor can
/// only appear in a file that changed, and a file that changed is in a crate the focus already
/// picked, so checking that crate catches it. What narrowing loses is a dependency of it that was
/// already broken, and that was already broken before this change.
fn msrv_scoped(scope: &[String]) -> Result<(), String> {
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
    let mut args = vec!["run".to_string(), version.clone(), "cargo".into(), "check".into()];
    args.extend(scope.iter().cloned());
    args.push("--all-features".into());
    println!("rustup {}", args.join(" "));
    let status = Command::new("rustup")
        .args(&args)
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

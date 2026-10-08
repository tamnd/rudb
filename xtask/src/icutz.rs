//! Fetching the IANA time zone release that DuckDB's ICU data holds and writing it into the tree.
//!
//! PostgreSQL reads the `tzdata.zi` of its own source tree, which `pg-vendor` copies. DuckDB reads
//! the zones that its copy of ICU was built with, which is an IANA release of its own, compiled
//! from the rearguard form of the data. That release is in the DuckDB binary as a string such as
//! `2026c`. This builds the `tzdata.zi` of that release in the rearguard form, the way the IANA
//! makefile builds it, so that `rudb-common` compiles both dialects' zones with one compiler.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use crate::sha256;

const UPSTREAM: &str = "https://data.iana.org/time-zones/releases";
/// Where the vendored file lives, relative to the workspace root.
const DEST: &str = "crates/rudb-common/vendor-icu";
const FILES: [&str; 2] = ["tzdata.zi", "LICENSE"];

/// Fetches the data of an IANA release and rewrites the vendored file from it.
pub(crate) fn vendor(release: Option<&str>) -> Result<(), String> {
    let Some(release) = release else {
        return Err("usage: cargo xtask icu-tz-vendor <IANA release, such as 2026c>".to_string());
    };
    if !release.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("{release} is not the name of an IANA release"));
    }
    let root = crate::root();
    let work = root.join("target").join("vendor-icu-tz");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)
        .map_err(|e| format!("could not make {}: {e}", work.display()))?;
    let tarball = format!("tzdata{release}.tar.gz");
    println!("fetching {UPSTREAM}/{tarball}");
    run(
        Command::new("curl").args(["-sSfL", "-o", &tarball, &format!("{UPSTREAM}/{tarball}")]),
        &work,
    )?;
    run(Command::new("tar").args(["xzf", &tarball]), &work)?;
    // The data tarball has no code, so the version file must not be remade from it.
    run(
        Command::new("make").args(["-s", "VERSION_DEPS=", "DATAFORM=rearguard", "tzdata.zi"]),
        &work,
    )?;

    let dest = root.join(DEST);
    std::fs::create_dir_all(&dest).map_err(|e| format!("could not make {DEST}: {e}"))?;
    let mut sums = String::new();
    for name in FILES {
        let data = std::fs::read(work.join(name))
            .map_err(|e| format!("could not read {name} of {tarball}: {e}"))?;
        std::fs::write(dest.join(name), &data)
            .map_err(|e| format!("could not write {DEST}/{name}: {e}"))?;
        let _ = writeln!(sums, "{}  {name}", sha256::hex(&data));
    }
    let manifest = format!(
        "# The IANA time zone data of DuckDB's ICU, built from the release tarball with\n\
         # `make VERSION_DEPS= DATAFORM=rearguard tzdata.zi`. `cargo xtask icu-tz-vendor` is the\n\
         # only thing that writes here and `cargo xtask icu-tz-check` checks that nothing else did.\n\
         #\n\
         # License: public domain, see LICENSE.\n\
         \n\
         upstream: {UPSTREAM}/{tarball}\n\
         release: {release}\n\
         \n\
         # sha256 of every vendored file, relative to this directory.\n\
         {sums}"
    );
    std::fs::write(dest.join("VENDOR"), manifest)
        .map_err(|e| format!("could not write {DEST}/VENDOR: {e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    println!("vendored the time zones of IANA {release} in the rearguard form");
    Ok(())
}

/// Checks that the vendored files are the ones `VENDOR` records.
pub(crate) fn check() -> Result<(), String> {
    let dest = crate::root().join(DEST);
    let text = std::fs::read_to_string(dest.join("VENDOR"))
        .map_err(|e| format!("could not read {DEST}/VENDOR: {e}"))?;
    let recorded: Vec<(&str, &str)> = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once("  "))
        .collect();
    let mut problems = Vec::new();
    for name in FILES {
        match (recorded.iter().find(|(_, file)| *file == name), std::fs::read(dest.join(name))) {
            (None, _) => problems.push(format!("{DEST}/{name} is not in VENDOR")),
            (_, Err(_)) => problems.push(format!("{DEST}/{name} is in VENDOR and is not there")),
            (Some((sum, _)), Ok(data)) if sha256::hex(&data) != *sum => {
                problems.push(format!("{DEST}/{name} is not the file VENDOR records"));
            }
            _ => {}
        }
    }
    let present = std::fs::read_dir(&dest).map_err(|e| format!("could not list {DEST}: {e}"))?;
    for entry in present.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != "VENDOR" && !FILES.contains(&name.as_str()) {
            problems.push(format!("{DEST}/{name} is in the tree and not in VENDOR"));
        }
    }
    if problems.is_empty() {
        println!("the time zones of DuckDB's ICU match VENDOR");
        return Ok(());
    }
    for problem in &problems {
        eprintln!("  {problem}");
    }
    Err(format!(
        "{} problems with the vendored time zones of DuckDB's ICU\n  \
         run `cargo xtask icu-tz-vendor <release>` and commit the result. Do not edit a vendored \
         file by hand",
        problems.len()
    ))
}

fn run(command: &mut Command, dir: &Path) -> Result<(), String> {
    let status =
        command.current_dir(dir).status().map_err(|e| format!("could not run {command:?}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{command:?} failed with {status}")) }
}

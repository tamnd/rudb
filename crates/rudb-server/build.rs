//! Records the target, the pointer width and the version of rustc for the text of `version()`.

use std::env;
use std::process::Command;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    let bits = env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default();
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    // `rustc --version` prints `rustc 1.90.0 (1159e78c4 2025-09-14)`, and the second word is the
    // version.
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.split_whitespace().nth(1).map(str::to_owned))
        .unwrap_or_default();
    println!("cargo:rustc-env=RUDB_TARGET={target}");
    println!("cargo:rustc-env=RUDB_POINTER_WIDTH={bits}");
    println!("cargo:rustc-env=RUDB_RUSTC={version}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC");
}

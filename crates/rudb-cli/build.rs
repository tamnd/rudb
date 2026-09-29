//! Links the shell as a position dependent executable on Linux, with the code queries run at
//! the front.
//!
//! A position independent executable has its pointer tables relocated by the loader at every
//! start. Over this binary that is about forty thousand relocations, a 950 KiB table the loader
//! reads through and 1.2 MiB of `.data.rel.ro` it writes to, and every page of both counts toward
//! the process's resident size before it has read a byte of any database. Linked at a fixed address
//! the linker resolves them once, the tables are ordinary read only pages of the file, and only the
//! ones a query touches are ever brought in. Document 32 measures it at 1.8 MiB off every query's
//! peak, which at ten million rows is most of the distance between the floor and the target.
//!
//! The cost is that the shell's own code is not placed at a random address. Its libraries, heap and
//! stack still are, and a program embedding the library is linked however that program chooses.
//! Only the binaries of this package are affected, so the C library and the tests keep the
//! toolchain's default.
//!
//! The code gets the same treatment. A fault on a page of it maps the fifteen around it too, and in
//! the order the compiler emits functions the ones a query runs are spread over most of the binary,
//! so `SELECT COUNT(*)` had 7 MiB of code resident to run 1.4 MiB of it. `hot-text.ld` gathers the
//! functions the ClickBench queries run into one section at the front, those most queries run first,
//! and takes 3 to 4 MiB off every query's peak. `scripts/text-order` is how it is written. It is
//! passed only where LLD is the linker the toolchain picks and nothing has named another, since that
//! is the linker it was measured with.
fn main() {
    let target = |key: &str| std::env::var(format!("CARGO_CFG_TARGET_{key}")).unwrap_or_default();
    if target("OS") == "linux" {
        println!("cargo:rustc-link-arg-bins=-no-pie");
    }
    let flags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    let default_linker = std::env::var_os("RUSTC_LINKER").is_none()
        && !flags.split('\u{1f}').any(|flag| {
            ["linker=", "linker-flavor", "linker-features", "fuse-ld"]
                .iter()
                .any(|option| flag.contains(option))
        });
    if target("OS") == "linux"
        && target("ARCH") == "x86_64"
        && target("ENV") == "gnu"
        && default_linker
    {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("hot-text.ld");
        println!("cargo:rustc-link-arg-bins=-Wl,-T,{}", script.display());
        println!("cargo:rerun-if-changed=hot-text.ld");
    }
    println!("cargo:rerun-if-changed=build.rs");
}

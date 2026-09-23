//! Links the shell as a position dependent executable on Linux.
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
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux") {
        println!("cargo:rustc-link-arg-bins=-no-pie");
    }
    println!("cargo:rerun-if-changed=build.rs");
}

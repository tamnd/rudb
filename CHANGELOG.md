# Changelog

Notable changes, newest first. This project is pre-1.0 and follows no compatibility promise until it has one, per `spec/18-package-layout.md`. The storage format version is stated in every release, because a file outlives the build that wrote it.

## Unreleased

Nothing yet.

## 0.1.0

The skeleton. There is no database here, and the point of tagging it is that the apparatus which measures the database exists before the database does.

- The workspace: 26 library crates and the `rudb` shell, each with an assigned rank in `xtask/layers.toml`. A crate may depend only on crates of strictly lower rank, and `cargo xtask layers` enforces it as a required CI job rather than as a convention.
- `cargo xtask style`, which checks the prose rules in CONTRIBUTING.md over every markdown file: no em or en dashes, no horizontal rules, no sentence broken across two lines.
- `cargo xtask ci`, which runs what the per-commit gate runs, in the order it runs it, so a contributor finds out on their own machine rather than on a pull request.
- The shell's argument handling. `rudb --version` and `rudb --print-config` work and nothing else does, which the help text says plainly.
- CI on Linux, macOS and Windows, debug and release, with a minimum supported Rust version job that reads its floor from `Cargo.toml`, a reproducible build check, and `cargo deny` over licenses, advisories, banned crates and unknown registries.
- The specification, twenty documents, in `spec/`. Written before the code on purpose.

Storage format version: none written yet.

# Changelog

Notable changes, newest first. This project is pre-1.0 and follows no compatibility promise until it has one, per `spec/18-package-layout.md`. The storage format version is stated in every release, because a file outlives the build that wrote it.

The version number says how far through the plan we are. **The minor version is the number of milestones finished**, so 0.0.y is work inside M0, 0.1.0 is the release where M0's exit criterion passed, 0.1.y is work inside M1, and so on. Patch releases happen whenever enough has landed to be worth a tag, which in practice is every few pull requests. The one exception is at the end: when M10 closes the version is 1.0.0 rather than 0.11.0, because that milestone is named 1.0 and pretending otherwise would be silly. The milestones are the issues at https://github.com/tamnd/rudb/issues.

## Unreleased

Nothing yet.

## 0.0.1

The skeleton. There is no database here, and the point of tagging it is that the apparatus which measures the database exists before the database does.

- The workspace: 26 library crates and the `rudb` shell, each with an assigned rank in `xtask/layers.toml`. A crate may depend only on crates of strictly lower rank, and `cargo xtask layers` enforces it as a required CI job rather than as a convention.
- `cargo xtask style`, which checks the prose rules in CONTRIBUTING.md over every markdown file: no em or en dashes, no horizontal rules, no sentence broken across two lines.
- `cargo xtask ci`, which runs what the per-commit gate runs, in the order it runs it, so a contributor finds out on their own machine rather than on a pull request.
- The shell's argument handling. `rudb --version` and `rudb --print-config` work and nothing else does, which the help text says plainly.
- CI on Linux, macOS and Windows, debug and release, with a minimum supported Rust version job that reads its floor from `Cargo.toml`, a reproducible build check, and `cargo deny` over licenses, advisories, banned crates and unknown registries.
- The specification, twenty documents, in `spec/`. Written before the code on purpose.

Storage format version: none written yet.

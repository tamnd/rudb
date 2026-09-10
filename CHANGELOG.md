# Changelog

Notable changes, newest first. This project is pre-1.0 and follows no compatibility promise until it has one, per `spec/18-package-layout.md`. The storage format version is stated in every release, because a file outlives the build that wrote it.

The version number says how far through the plan we are. **The minor version is the number of milestones finished**, so 0.0.y is work inside M0, 0.1.0 is the release where M0's exit criterion passed, 0.1.y is work inside M1, and so on. Patch releases happen whenever enough has landed to be worth a tag, which in practice is every few pull requests. The one exception is at the end: when M10 closes the version is 1.0.0 rather than 0.11.0, because that milestone is named 1.0 and pretending otherwise would be silly. The milestones are the issues at https://github.com/tamnd/rudb/issues.

## Unreleased

Nothing yet.

## 0.0.1

The skeleton and the two interfaces underneath everything else. There is no database here. You cannot run a query. The point of tagging it is that the apparatus which measures the database exists before the database does, and that the two widest interfaces in the system are settled before there is anything built on top of them to make changing them expensive.

- The workspace: 26 library crates and the `rudb` shell, each with an assigned rank in `xtask/layers.toml`. A crate may depend only on crates of strictly lower rank, and `cargo xtask layers` enforces it as a required CI job rather than as a convention.
- `rudb-common`: the error model, the type system and the single value. Errors map to DuckDB's exact message prefixes and a `Result` is the same width as the value in it. Every DuckDB logical type is present, prints the way DuckDB prints it, and parses back from that spelling with the aliases handled. Values print the way DuckDB prints them, which is mostly a long tail of small things like a double printing as `1` and not `1.0`.
- `rudb-vector`: the vector interface. Four physical forms, flat, constant, sequence and dictionary. Validity in three cases rather than a bitmap that happens to be all ones. Selection vectors instead of compaction. The 16 byte string view with the 4 byte prefix. Specified in `spec/07-execution.md` section 7.1 as the widest interface in the system, which is why it is written before the first operator instead of after the fifth.
- `cargo xtask style`, which checks the prose rules in CONTRIBUTING.md over every markdown file: no em or en dashes, no horizontal rules, no sentence broken across two lines.
- `cargo xtask msrv`, which builds the workspace against the oldest Rust the manifest claims. A language feature newer than the floor does not announce itself, it just compiles on whatever toolchain is installed, so this is the only check that finds one.
- `cargo xtask ci`, which runs what the per-commit gate runs, in the order it runs it, so a contributor finds out on their own machine rather than on a pull request. Twice now the local gate has turned out to be weaker than the remote one, and both times the fix was to close the gap here rather than to remember harder.
- The shell's argument handling. `rudb --version` and `rudb --print-config` work and nothing else does, which the help text says plainly.
- CI on Linux, macOS and Windows, debug and release, with a minimum supported Rust version job that reads its floor from `Cargo.toml`, a reproducible build check, and `cargo deny` over licenses, advisories, banned crates and unknown registries.
- The specification, twenty documents, in `spec/`. Written before the code on purpose.

Known deviation from the specification: `StringView` stores a block index and an offset where section 7.1 says pointer. Same size, same prefix trick, no pinning machinery required, and tracked as issue #16 so that M3 measures the two instead of inheriting whichever was written first.

Storage format version: none written yet.

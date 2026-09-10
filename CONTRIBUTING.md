# Contributing

Thanks for looking. This document is short because most of what a contributor needs to know is in [`spec/`](spec/), and this is only the part about how changes get made.

## Before you start

The project is at M0. There is a lot of design written down and very little code, which means the highest value contribution right now is reading a specification document and telling us where it is wrong. [`spec/19-open-questions.md`](spec/19-open-questions.md) is the ranked list of things that are not decided, and an argument against one of the answers there is worth more than a patch. Open an issue with `kind/open-question` if something does not hold up.

If you want to write code, take a milestone issue or a piece of one, and say so on the issue first. The milestones are ordered so that the parts which could kill the project come first, and work on M4 before M2 exists is work that gets thrown away.

## Versions and releases

The minor version is the number of milestones finished. Work inside M0 is tagged 0.0.1, 0.0.2 and so on, the release where M0's exit criterion passes is 0.1.0, work inside M1 is 0.1.1 upwards, and M1 closing is 0.2.0. When M10 closes the version is 1.0.0 rather than 0.11.0, because that milestone is named 1.0.

A patch release goes out whenever enough has landed to be worth a tag. There is no schedule and no release branch. Tagging is the whole process: push a tag that matches the version in `Cargo.toml`, and the release workflow checks the tag against the manifest, checks that CHANGELOG.md has a section for it, runs the full gate, builds the five targets, and publishes. If any of those fail there is no release, which is the point of doing it that way rather than by hand.

Every release states its storage format version, including the releases that do not write files, where it states that it does not write files. A database file outlives the build that wrote it and the release notes are where somebody finds out whether their file is still readable.

## Running the checks

```
cargo xtask ci
```

That runs the checks the per-commit CI job runs, cheapest first, so a formatting mistake costs you seconds rather than a full test run.

The individual pieces:

```
cargo xtask layers      # the dependency graph against xtask/layers.toml
cargo xtask style       # prose against the house rules
cargo xtask msrv        # the workspace still builds on the oldest Rust the manifest claims
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
```

The minimum supported Rust version check needs that toolchain installed, which is `rustup toolchain install 1.85.0 --profile minimal`. If it is not installed the task says so and continues rather than failing, because CI runs it either way and the point is to find out sooner, not to make a fresh clone unbuildable. It catches one thing that nothing else catches: a language feature newer than the floor, which does not announce itself, it just compiles on whatever toolchain is in front of you.

## What a change has to come with

**A change to behavior comes with a test that fails without it.** Not a test that exercises the code, a test that fails. If you cannot write one, say so in the pull request and explain why, and we will work out what the right test is together.

**A change that could return a different answer comes with a case in the differential corpus.** A wrong answer is the worst thing this project can ship, worse than a crash, because a crash is noticed. [`spec/16-testing.md`](spec/16-testing.md) section 16.9 says a release has no known wrong-answer bug, and the way that stays true is that every fix arrives with the query that found it.

**A new encoded fast path comes with the equivalence test that runs it against the decoded path.** This is the specific hazard of the design in [`spec/06-compression.md`](spec/06-compression.md) section 6.7: an operator that runs on dictionary codes and an operator that runs on decoded values are two implementations of one thing, and two implementations of one thing disagree eventually. Section 16.2 says they are tested against each other over generated data rather than reviewed for agreement, and that is not negotiable no matter how obvious the kernel looks.

**A performance claim comes with the command that reproduces it.** [`spec/15-rudb-bench.md`](spec/15-rudb-bench.md) section 15.1 has ten reporting rules and they apply to pull request descriptions as much as to the README. Median of at least five runs with the interquartile range, never a minimum, cold and hot separately, the full table including the losses, and the machine described. Load time and on-disk size belong next to every runtime number, because a runtime win paid for with a 10x load is a different product than it looks.

**A change to the storage format comes with a version bump and a round trip test.** A file written by a build in the wild outlives the build. Writing the same data twice has to produce the same bytes, which means no hash iteration order and no timestamps in the output.

**A new dependency comes with a row in the table in [`spec/18-package-layout.md`](spec/18-package-layout.md).** Adding one should be a reviewable decision, which is what the table is for. The budget is under forty crates including transitives for the database itself, and `cargo deny` enforces the license and advisory side of it.

**A new `#[ignore]` or corpus exclusion comes with an issue number.** No test is deleted to make CI green. It is marked, given an issue, and counted in a report that is visible.

## The layer rule

The workspace is 27 crates and each has a rank in `xtask/layers.toml`. A crate may depend only on crates of strictly lower rank, and `cargo xtask layers` is a required CI job. If your change needs an edge that goes the wrong way, that is a design conversation and not a rank edit. Usually the answer is that a type belongs further down than where it currently lives.

## Style

**Rust.** `cargo fmt` decides layout, so there is nothing to argue about there. Beyond that: comments explain why, not what, and a comment that restates the line above it is worse than no comment. Public items get documentation. Anything with a precondition gets a `# Panics` section, and `clippy::undocumented_unsafe_blocks` means every `unsafe` block gets a safety comment saying what invariant makes it sound.

Unsafe is allowed in five crates and forbidden in the rest, which the crate roots say for themselves. Those five are where the buffer manager hands out pointers into mapped pages, where the vector layer indexes without bounds checks behind a validity mask that has already been consulted, where the kernels use `core::arch` intrinsics, and where the C API takes a pointer from a caller. If you find yourself wanting `unsafe` outside them, that is a signal about where the code belongs.

Where a decision in the code follows from something in the specification, cite the document. `// per spec/06-compression.md section 6.7` costs one line and saves the next person an afternoon.

**Prose.** README, specification documents, commit messages, issue and pull request text. Plain English, written the way you would explain it to a colleague. No em dashes and no en dashes: a comma, a colon, a period, parentheses or the word "to" all work and one of them is always right. No horizontal rules; use a heading. Do not hard-wrap sentences across lines, because a one-line-per-paragraph file produces readable diffs and a wrapped one does not. `cargo xtask style` checks all three of those mechanically.

Publish the losses next to the wins. A benchmark table with the regressions removed is not a benchmark table.

## Commits and pull requests

One logical change per commit. The subject line is imperative and under about seventy characters, the body says why rather than what, because what is in the diff.

Pull requests describe the problem, then the change, then how it was verified. If it touches performance, the numbers go in the description. If it touches the layer graph, say which rank changed and why.

Rebase rather than merge. The history is meant to be bisectable, and a performance regression is found by bisecting far more often than by reading.

## Reporting a wrong answer

This is the most valuable kind of bug report the project can get, so it gets its own issue template and its own handling.

Reduce it if you can. A schema, a few rows and one `SELECT` is a bug report where a fifty-table warehouse is a project. If you cannot reduce it, file it anyway and we will reduce it. Say whether it survives `SET threads = 1` and whether it survives with the optimizer disabled, because each of those cuts the search in half.

If DuckDB and PostgreSQL agree with each other and disagree with us, say so. That settles which engine is wrong before anyone opens a debugger.

## Security

See [SECURITY.md](SECURITY.md). A crash on a malformed database file is a bug and possibly worse, because people open files they did not create.

## License

The project is under Apache-2.0 and a contribution is offered under the same terms, which is what section 5 of the license says about a contribution submitted for inclusion in the work. There is no separate agreement to sign.

# rudb

An embedded analytical database written in Rust, compatible with DuckDB.

Its own columnar format with global dictionaries and multi-column compression, and operators that run directly on the encoded data instead of decoding it first. The bar is ten times DuckDB on ClickBench at half the disk, and every part of that sentence is a number in [`spec/02-the-goal.md`](spec/02-the-goal.md) that can come out wrong. The disk half of it already has: it said a tenth until the format was built and measured.

This is early, and it runs. All 43 ClickBench queries and all 22 TPC-H queries return DuckDB's answer, there is a native storage format behind a shell with DuckDB's command line, and rudb is a column on the benchmark board next to DuckDB, ClickHouse, DataFusion and Polars rather than a row saying not built. What it is not yet is fast enough, and the [live board](https://tamnd.github.io/rudb-bench/) is where that shows: it wins under a hundred thousand rows and loses above it. The full technical design is written down in [`spec/`](spec/) before it is built, and the milestones that build it are tracked as issues.

## Why another analytical database

DuckDB is very good and it is the thing to beat, not the thing to complain about. It made single node analytics normal, it is a pleasure to use, and its SQL dialect is the most productive one anybody ships. The gap this project is aiming at is not usability, it is two numbers.

On ClickBench at `c6a.4xlarge`, summing the best hot run of each of the 43 queries, DuckDB is at 26.25 seconds and Umbra is at 8.10 seconds. The same dataset takes 20.46 GB in DuckDB's format and 8.30 GB in Umbra's. Those are both large gaps between a system people use and a system that exists, and neither gap is explained by DuckDB doing anything wrong. It is explained by a set of choices about physical layout that were reasonable when they were made and that the last three years of research have moved past.

In March 2026 Wehrstein, Eckmann, Jasny and Binnig published [Bespoke OLAP](https://arxiv.org/abs/2603.02001), which synthesized a query engine specialized to a fixed workload and measured 11.17x over DuckDB on TPC-H. The headline is not the useful part. The ablation is: constrained to a flat columnar layout and allowed to specialize only its code, the same pipeline got 1.26x on TPC-H and 0.57x on the CEB join workload, meaning it lost. Allowed to specialize the storage layout too, it got 12.35x and 51.40x.

Compilation, fusion, unrolling and prefetch on top of a conventional layout bought about a quarter. Choosing a different physical representation for the data bought an order of magnitude. Every design decision here follows from that inversion, which is why the two longest documents in the specification are the storage and compression ones and why the execution document is written as "fast enough not to be the bottleneck" rather than as the source of the win.

rudb is aiming at four things at once, stated as falsifiable claims rather than aspirations:

1. **Compatibility.** DuckDB v2.0 files read and written at full fidelity, the SQL dialect at a published weighted coverage, the C API such that a program built against `duckdb.h` links and runs, and extensions loading. Four named levels rather than one binary claim, each with a test suite that produces its own status table.
2. **Aggregate performance.** 10x on total ClickBench hot runtime and on the ClickBench Combined metric against DuckDB, same instance, untuned. 10x on TPC-H at SF100. 5x or better on TPC-DS, JOB and CEB, where the bound is planner robustness rather than scan throughput.
3. **Per-query floor.** No query slower than DuckDB, on any suite, ever. That clause is harder than the aggregate one and it is the one that keeps the design honest, because most ways of winning on average lose badly somewhere.
4. **Resource.** 2.1x smaller on disk, 10x lower peak resident set, 10x fewer CPU-seconds. This is the axis nobody publishes and it is the one that decides whether the engine is actually better or is merely trading memory for time. The disk figure said 10x until M1 went and measured it: a standalone encoder over the real `hits` came out at 9.65 GB against DuckDB's 20.46, not the roughly 2 GB the design had assumed, so the claim was amended to what the measurement supports rather than left standing. [`spec/02-the-goal.md`](spec/02-the-goal.md) section 2.6.1 is the amendment and [`reports/2026-09-11/m1-the-format-experiment.md`](https://github.com/tamnd/rudb-bench/blob/main/reports/2026-09-11/m1-the-format-experiment.md) in rudb-bench is the measurement. The other two numbers on this axis have not been measured that way and are still claims.

[`spec/15-rudb-bench.md`](spec/15-rudb-bench.md) fixes the methodology and the reporting rules before there is anything to report, which is the only order in which those rules mean anything. [`spec/03-baselines.md`](spec/03-baselines.md) has the full 43-query table the claims are measured against, recomputed from the official result files rather than taken from anybody's slide.

## Design in one page

**The physical layout is chosen at runtime and re-chosen over time.** Each column gets a representation from a small closed set, picked by a sampling encoder at write time and revised later by a background recompressor that has seen what the queries actually ask for. This is the project thesis and it is where the order of magnitude is supposed to come from.

**Execution runs on encoded data wherever the encoding permits it.** A dictionary encoded column is grouped on its codes, so the group key is four bytes instead of a string. A frame-of-reference column is filtered by transforming the predicate rather than the data. A run-length encoded column is aggregated by arithmetic over the runs. An FSST column is searched in the compressed domain. Decoding is the fallback path, not the default. The obvious risk is that a fast path and a slow path disagree, so [`spec/16-testing.md`](spec/16-testing.md) section 16.2 makes every encoded kernel run against its decoded twin on the same data as a test rather than as an audit.

**Compression is across columns, not only within them.** ClickBench `hits` has 105 columns and a great many of them are functions of each other. A format that compresses each column in isolation cannot see that, and single-column encoding is where the state of the art already is, so it is not where a 10x resource claim can come from. Global dictionaries shared across row groups, shared symbol tables, and recomputation rules are the mechanism. Whether the correlations are actually there in real data was open question one, and it was measured before anything was built on top of it: a standalone encoder over the real `hits` reached 9.65 GB against DuckDB's 20.46, which is a genuine halving and is not the order of magnitude the design had assumed. The claim was amended to the measurement. That is the useful outcome of asking the expensive question first.

**Vectors are 8192 values, after that constant was measured rather than argued about.** It was 1024, because 1024 is the [FastLanes](https://www.vldb.org/pvldb/vol18/p4629-afroozeh.pdf) unit and matching it lets the compression layer and the execution layer share a granularity. The argument was at the wrong level. The same constant was also deciding how much of a table one zone map covers and how much work one call into the pipeline does, and both of those want a far larger number than the encoding alignment does. Once a table in memory had row groups of its own, the storage question stopped depending on the vector, and five release binaries differing only in `VECTOR_SIZE` settled what was left: every scan number improves monotonically up to 8192 and turns back up at 32768, where a copied vector stops fitting in L2. The compatibility layer converts at the boundary either way.

**Four execution tiers, chosen per pipeline.** Interpreted vectorized for short queries, a fused path for the common operator chains, Cranelift for long pipelines, and a hand-written single-pass emitter if and only if the experiment in [`spec/08-codegen.md`](spec/08-codegen.md) shows Cranelift's compile latency is the binding constraint. Only the first tier exists today, which `--print-config` says out loud, and building tier 3 speculatively is how projects spend a year on a backend nothing needed.

**Robust Predicate Transfer in the planner by design, not bolted on later.** An engine that gets join ordering right by being robust rather than by having a better cardinality estimator is a more defensible design, and it is the only credible path to the JOB and CEB numbers.

**A layer rule that is checked.** The workspace is 33 crates with an assigned rank, and a crate may depend only on strictly lower ranks. `cargo xtask layers` is a required CI job and prints what it proved. This is what makes "the optimizer cannot see the parser" a fact about the build rather than a claim in a document.

**Every layer has a textual form and a round-trip parser.** Logical plan, physical plan, encoded chunk metadata, compiled pipeline IR. Every stage can be dumped, diffed, fuzzed and bisected on its own. That is what modular cashes out to, and it is the mechanism by which a new result from a paper can be dropped into one crate and measured without touching the others.

## Status

The minor version says how far through the plan the engine is. **It counts finished milestones**, and from 0.3.0 the milestones it counts are the F series in [`spec/engine-v2/16-milestones.md`](spec/engine-v2/16-milestones.md) rather than the M series that came before. 0.3.0 is the release where F0's exit criterion passed, 0.3.y is work inside F1, 0.4.0 is the release where F1 closes, and so on up to F11, which gets 1.0.0 rather than 0.14.0 because pretending otherwise would be silly. The count does not restart at the handover: 0.0.y through 0.2.y were the M series, where 0.1.0 closed M0 and 0.2.0 closed M1, and a version number cannot go backwards. [`CHANGELOG.md`](CHANGELOG.md) states the rule and the storage format version of every release, because a file outlives the build that wrote it.

**F0 is closed.** Its gate was that all 43 ClickBench queries and all 22 TPC-H queries return DuckDB's answer, that `EXPLAIN ANALYZE` renders, that the metrics cross-check passes, and that rudb is a real engine in rudb-bench with a column on the board that is published even though it is last. That is what shipped, and the last clause of it was the point: the baseline every later ratio is a ratio against has to exist before there is anything to flatter.

```
$ rudb
rudb 0.3.72
Enter ".help" for usage hints.
D CREATE TABLE t(x INTEGER, name VARCHAR);
D INSERT INTO t VALUES (6, 'row 6'), (7, 'row 7'), (2, 'row 2');
D SELECT * FROM t WHERE x > 5;
┌───────┬─────────┐
│   x   │  name   │
│ int32 │ varchar │
├───────┼─────────┤
│     6 │ row 6   │
│     7 │ row 7   │
└───────┴─────────┘
```

The command line and the sixteen output modes are DuckDB's, diffed against a real `duckdb` binary rather than described from memory. Behind it there is a native storage format, a Parquet reader, an optimizer of nineteen passes whose plans are written down and checked on every commit, statistics a table keeps as its rows arrive, and encoded kernels that run on dictionary codes and packed integers rather than on decoded values. Queries run on more than one core, which they did not at 0.3.5. What is not there is the thing the ten-times claim rests on, which is F7, where running on codes rather than on decoded values becomes the path every operator takes instead of a handful of kernels. [`spec/engine-v2/16-milestones.md`](spec/engine-v2/16-milestones.md) is the list of what each of F1 through F11 still owes and the gate each one has to pass to be called done.

```
$ rudb --print-config
version: 0.3.72
memory-limit: 19660MiB
threads: 10
query-timeout: none
vector-size: 8192
row-group-size: 122880
storage-format: native (rudb v1), DuckDB import and export
execution-tiers: interpreted
duckdb-compat-level: none claimed, spec/12-duckdb-compat.md
target: aarch64
os: macos
```

### Where the performance actually is

Against DuckDB on ClickBench and TPC-H, at every size from a thousand rows to ten million, on one machine on one afternoon: [**the board**](https://tamnd.github.io/rudb-bench/), which is generated from committed measurements rather than written. It is not repeated here, because a number copied into a second README is a number that goes stale in a place nobody re-renders.

What it currently says, and what the shape of it means, is worked through in [`spec/perf/`](spec/perf/). The short version is that rudb is not losing to any one slow loop. It was losing to four things that multiply, and no single one of them closes the gap: one thread where DuckDB used six, Parquet decoded inside every query instead of a native format read once at load, a group key that is a `Vec<Value>` on the heap, and a fixed per-query floor. Those are F4, F2, F5 and F2 again, and [`spec/perf/03-roadmap.md`](spec/perf/03-roadmap.md) re-orders the milestones by which of them the measurement says to do first rather than by the order they were written in, which put the largest multiplier fifth.

The first of the four is no longer true, which is the F4 work: at a million ClickBench rows rudb now averages 3.15 cores against DuckDB's 2.35, where at 0.3.5 it averaged 0.99 against 9.01. The other three still are, and the board says so in the shape a fixed cost and a per-row cost make together. rudb is ahead of DuckDB up to a hundred thousand rows and behind it above that, which is what an engine with a low floor and an expensive per-row path looks like.

The milestone document ends with the arithmetic rather than with an assertion, and it is worth repeating here because it is the part a reader should hold us to. F1 is a large multiple and almost none of it counts, because it is a comparison against ourselves. F2 through F6 get rudb from "an engine" to "a competitive engine", which is a factor of one against the claim at the top of this file. **F7, encoded execution, is the claim**: three mechanisms, 9x to 11x if all three land and 4x to 6x if one fails. F8 and F10 are the twenty-six per cent that the literature prices the whole execution-engineering story at, which is real and is not an order of magnitude. If F7 lands at 4x, the project ends at about 5x, which is a good engine and a failed claim, and [`spec/02-the-goal.md`](spec/02-the-goal.md) section 2.7 already says the response is to publish that sentence rather than to adjust the benchmark.

## Building

```
git clone https://github.com/tamnd/rudb
cd rudb
cargo build --release
```

That is the whole of it, on Linux, macOS and Windows. No CMake, no Python in the build, no code generation step that is not a `build.rs` or an `xtask`. Everything else is a task:

```
cargo xtask layers    # check the dependency graph against xtask/layers.toml
cargo xtask style     # check the prose against the house rules
cargo xtask bench     # time the front end against a frozen workload, as a table
cargo xtask bench smoke  # the whole comparison, against every engine on this machine
cargo xtask smoke     # run a query end to end on this host and check the answers
cargo xtask ci        # run what CI runs, in the order CI runs it
```

`cargo xtask bench <suite>` is the one that produces a table with somebody else in it. It builds rudb and it builds [rudb-bench](https://github.com/tamnd/rudb-bench), which it expects to find checked out beside this repository or wherever `RUDB_BENCH_REPO` says, and then runs the suite against every engine the machine has. DuckDB, ClickHouse, DataFusion and Polars are each a row if they are installed and a line saying they are not if they are not. `smoke` generates its own data and is not comparable to anything. The suites that are comparable need a download or a generator, and `rudb-bench suites` says which.

## Repository layout

```
crates/         the database, 32 library crates plus the shell
spec/           the technical design, written before the code
xtask/          build automation, including the layer rule and the prose check
```

[`spec/18-package-layout.md`](spec/18-package-layout.md) explains the rank of each crate and why it is where it is. The two sibling repositories are [`tamnd/rudb-compat`](https://github.com/tamnd/rudb-compat), which is the differential harness against a real DuckDB, and [`tamnd/rudb-bench`](https://github.com/tamnd/rudb-bench), which is the benchmark harness. They are separate so that a result can be reproduced by someone who does not trust us, without building the engine from a specific commit of the engine's own repository.

## The specification

Written before the implementation, in the repository rather than a wiki because it is reviewed and revised in pull requests like everything else. It is in two parts. The twenty-one numbered documents at the top level are the design as a whole and change rarely. The subdirectories are the working specifications for the parts currently being built, and they change as the thing they describe is measured.

| | |
|---|---|
| [00](spec/00-README.md) | What this is, the settled decisions, what to read first |
| [01](spec/01-research-2026.md) | The research the design draws on, with citations |
| [02](spec/02-the-goal.md) | The four axes, stated as falsifiable claims |
| [03](spec/03-baselines.md) | Measured baselines, and where DuckDB's time actually goes |
| [04](spec/04-architecture.md) | Layers, dataflow, threading, errors and cancellation |
| [05](spec/05-storage.md) | The file format, block layout, zone maps, the buffer manager |
| [06](spec/06-compression.md) | Encodings, cascading, multi-column compression, encoded execution |
| [07](spec/07-execution.md) | Vectors, operators, morsels, hash tables, strings, spilling |
| [08](spec/08-codegen.md) | The four tiers, the expression IR, Cranelift, the fallback |
| [09](spec/09-optimizer.md) | Rewrites, predicate transfer, cardinality, layout adaptation |
| [10](spec/10-sql-and-types.md) | The type system, the DuckDB dialect surface, functions |
| [11](spec/11-transactions.md) | MVCC, WAL, checkpointing, the single-writer model, DDL |
| [12](spec/12-duckdb-compat.md) | Six compatibility surfaces and the four levels |
| [13](spec/13-ecosystem.md) | Parquet, Arrow, Iceberg, extensions, language clients |
| [14](spec/14-rudb-compat.md) | The differential harness and how a compatibility claim is earned |
| [15](spec/15-rudb-bench.md) | What we measure, against whom, and the rules for reporting it |
| [16](spec/16-testing.md) | Unit, property, fuzz, crash consistency, equivalence |
| [17](spec/17-milestones.md) | The original M0 to M11, superseded by the F series but kept |
| [18](spec/18-package-layout.md) | The crate tree, dependency rules, stability tiers |
| [19](spec/19-open-questions.md) | The ranked list that has to be answered, and by when |
| [20](spec/20-the-grammar.md) | The grammar the parser is generated from and checked against |

And the working folders, each one a numbered series of its own:

| | |
|---|---|
| [`perf/`](spec/perf/) | Where the time goes, what it costs to fix, and the order to fix it in. The roadmap here overrides 17's ordering |
| [`engine-v2/`](spec/engine-v2/) | The engine as it is being rebuilt, including [16-milestones.md](spec/engine-v2/16-milestones.md), which is the F series the version number counts |
| [`planner-v2/`](spec/planner-v2/) | The planner and execution pass currently in the code |
| [`storage-v3/`](spec/storage-v3/) | The native single-file format, its evidence, and its benchmark contract |
| [`stats/`](spec/stats/) | Statistics, metadata and the feedback loop that revises a layout |
| [`graph/`](spec/graph/) | The plan graph and what is allowed to rewrite it |
| [`engine/`](spec/engine/), [`planner/`](spec/planner/), [`storage-v2/`](spec/storage-v2/) | The previous passes, kept because the second pass is only legible next to what it replaced |
| [`bench/`](spec/bench/), [`sql/`](spec/sql/) | Reference material: the TPC-H queries and DuckDB's own documentation, vendored so a claim about the dialect can be checked against the source |

Read 02 first, then 03, then 01. Document 02 decides whether the project is honest, document 03 is the measurement it rests on, and document 01 is the literature that says the measurement is reachable. After those, `perf/03-roadmap.md`, because it is the one that says what is being worked on this week.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). The short version is that a change to behavior comes with a test that would fail without it, a new encoded fast path comes with the equivalence test that runs it against the decoded path, and a performance claim comes with the command that reproduces it.

## Not in scope

Distributed execution, until a condition is met that has not been met. A cluster is the wrong answer to a 2026 machine with 448 cores, and the project's claim is a single-node claim, so F11 in [`spec/engine-v2/16-milestones.md`](spec/engine-v2/16-milestones.md) is behind a trigger stated harshly on purpose: no work begins on it until the single-node ClickBench number is published and is better than DuckDB's. A distributed engine that is slower than DuckDB on one machine is not interesting, and finding distribution before finding speed is the characteristic way a project like this one fails. Not a transactional database, though it has MVCC and ACID because compatibility requires them. Not a lakehouse catalog and not a streaming system. Not a general dataframe library before 1.0. No GPU in the core, because a GPU-first engine gives up the runs-on-a-laptop property that is most of the reason anybody uses an embedded database. Each of those is argued where it belongs in the specification rather than quietly unmentioned.

## License

Apache-2.0. See [LICENSE-APACHE](LICENSE-APACHE).

Not affiliated with, endorsed by or derived from DuckDB Labs. The compatibility surface is reimplemented against the published format and the generated C header.

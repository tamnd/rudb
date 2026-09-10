# rudb

An embedded analytical database written in Rust, compatible with DuckDB.

Its own columnar format with global dictionaries and multi-column compression, and operators that run directly on the encoded data instead of decoding it first. The bar is ten times DuckDB on ClickBench at a tenth of the disk, and every part of that sentence is a number in [`spec/02-the-goal.md`](spec/02-the-goal.md) that can come out wrong.

This is early. Nothing answers a query yet. What exists is the workspace, the layer rule that keeps it modular, the shell skeleton, and CI that is green on Linux, macOS and Windows from the first commit. The full technical design is written down in [`spec/`](spec/) before it is built, and the milestones that build it are tracked as issues.

## Why another analytical database

DuckDB is very good and it is the thing to beat, not the thing to complain about. It made single node analytics normal, it is a pleasure to use, and its SQL dialect is the most productive one anybody ships. The gap this project is aiming at is not usability, it is two numbers.

On ClickBench at `c6a.4xlarge`, summing the best hot run of each of the 43 queries, DuckDB is at 26.25 seconds and Umbra is at 8.10 seconds. The same dataset takes 20.46 GB in DuckDB's format and 8.30 GB in Umbra's. Those are both large gaps between a system people use and a system that exists, and neither gap is explained by DuckDB doing anything wrong. It is explained by a set of choices about physical layout that were reasonable when they were made and that the last three years of research have moved past.

In March 2026 Wehrstein, Eckmann, Jasny and Binnig published [Bespoke OLAP](https://arxiv.org/abs/2603.02001), which synthesized a query engine specialized to a fixed workload and measured 11.17x over DuckDB on TPC-H. The headline is not the useful part. The ablation is: constrained to a flat columnar layout and allowed to specialize only its code, the same pipeline got 1.26x on TPC-H and 0.57x on the CEB join workload, meaning it lost. Allowed to specialize the storage layout too, it got 12.35x and 51.40x.

Compilation, fusion, unrolling and prefetch on top of a conventional layout bought about a quarter. Choosing a different physical representation for the data bought an order of magnitude. Every design decision here follows from that inversion, which is why the two longest documents in the specification are the storage and compression ones and why the execution document is written as "fast enough not to be the bottleneck" rather than as the source of the win.

rudb is aiming at four things at once, stated as falsifiable claims rather than aspirations:

1. **Compatibility.** DuckDB v2.0 files read and written at full fidelity, the SQL dialect at a published weighted coverage, the C API such that a program built against `duckdb.h` links and runs, and extensions loading. Four named levels rather than one binary claim, each with a test suite that produces its own status table.
2. **Aggregate performance.** 10x on total ClickBench hot runtime and on the ClickBench Combined metric against DuckDB, same instance, untuned. 10x on TPC-H at SF100. 5x or better on TPC-DS, JOB and CEB, where the bound is planner robustness rather than scan throughput.
3. **Per-query floor.** No query slower than DuckDB, on any suite, ever. That clause is harder than the aggregate one and it is the one that keeps the design honest, because most ways of winning on average lose badly somewhere.
4. **Resource.** 10x smaller on disk, 10x lower peak resident set, 10x fewer CPU-seconds. This is the axis nobody publishes and it is the one that decides whether the engine is actually better or is merely trading memory for time.

[`spec/15-rudb-bench.md`](spec/15-rudb-bench.md) fixes the methodology and the reporting rules before there is anything to report, which is the only order in which those rules mean anything. [`spec/03-baselines.md`](spec/03-baselines.md) has the full 43-query table the claims are measured against, recomputed from the official result files rather than taken from anybody's slide.

## Design in one page

**The physical layout is chosen at runtime and re-chosen over time.** Each column gets a representation from a small closed set, picked by a sampling encoder at write time and revised later by a background recompressor that has seen what the queries actually ask for. This is the project thesis and it is where the order of magnitude is supposed to come from.

**Execution runs on encoded data wherever the encoding permits it.** A dictionary encoded column is grouped on its codes, so the group key is four bytes instead of a string. A frame-of-reference column is filtered by transforming the predicate rather than the data. A run-length encoded column is aggregated by arithmetic over the runs. An FSST column is searched in the compressed domain. Decoding is the fallback path, not the default. The obvious risk is that a fast path and a slow path disagree, so [`spec/16-testing.md`](spec/16-testing.md) section 16.2 makes every encoded kernel run against its decoded twin on the same data as a test rather than as an audit.

**Compression is across columns, not only within them.** ClickBench `hits` has 105 columns and a great many of them are functions of each other. A format that compresses each column in isolation cannot see that, and single-column encoding is where the state of the art already is, so it is not where a 10x resource claim can come from. Global dictionaries shared across row groups, shared symbol tables, and recomputation rules are the mechanism. Whether the correlations are actually there in real data is open question one, and M1 measures it before anything is built on top of it.

**Vectors are 1024 values, not DuckDB's 2048.** 1024 is the [FastLanes](https://www.vldb.org/pvldb/vol18/p4629-afroozeh.pdf) unit, and matching it means the compression layer and the execution layer share a granularity. That is what makes compressed execution cheap instead of awkward. The compatibility layer converts at the boundary.

**Four execution tiers, chosen per pipeline.** Interpreted vectorized for short queries, a fused path for the common operator chains, Cranelift for long pipelines, and a hand-written single-pass emitter if and only if the M8 experiment shows Cranelift's compile latency is the binding constraint. Building tier 3 speculatively is how projects spend a year on a backend nothing needed.

**Robust Predicate Transfer in the planner from M4, not bolted on later.** An engine that gets join ordering right by being robust rather than by having a better cardinality estimator is a more defensible design, and it is the only credible path to the JOB and CEB numbers.

**A layer rule that is checked.** The workspace is 27 crates with an assigned rank, and a crate may depend only on strictly lower ranks. `cargo xtask layers` is a required CI job. This is what makes "the optimizer cannot see the parser" a fact about the build rather than a claim in a document.

**Every layer has a textual form and a round-trip parser.** Logical plan, physical plan, encoded chunk metadata, compiled pipeline IR. Every stage can be dumped, diffed, fuzzed and bisected on its own. That is what modular cashes out to, and it is the mechanism by which a new result from a paper can be dropped into one crate and measured without touching the others.

## Status

M0 is finished, as of v0.1.0. There is a parser for `SELECT`, a binder, a logical plan with a textual form that reads back, a catalog, an in-memory table, the tier 0 kernels and a tier 0 interpreter, so a query runs end to end:

```
$ cargo xtask smoke
SELECT * FROM t WHERE x > 5
  6, row 6
  7, row 7
  8, row 8
  9, row 9
  10, row 10
```

That is the query M0 exists to produce and it is the whole of what works, checked on macOS, Linux and Windows with 502 tests green on each. No storage format, no optimizer, no transactions, no `CREATE TABLE`, and every join is a nested loop. The shell is still an argument parser rather than a shell. M1 is next and it is the format experiment.

```
$ rudb --print-config
version: 0.1.0
vector-size: 1024
row-group-size: 122880
storage-format: native (rudb v1), DuckDB import and export
execution-tiers: interpreted
duckdb-compat-level: 0 (nothing is implemented yet)
target: aarch64
os: macos
```

The twelve milestones are in [`spec/17-milestones.md`](spec/17-milestones.md) and are tracked as issues. Three of them are places where stopping produces something useful. M3 is a correct, compatible engine at roughly Umbra-class speed with a much smaller footprint. M6 adds durability under crash and stability under memory pressure, and is the first version anybody should point at data they care about. M10 is the claim at the top of this file, or a published and honest restatement of it.

The order is deliberate: the parts that could kill the project come first. M1 is a measurement rather than a feature, and if it says the compression ratios are not reachable then the specification is amended before a storage engine is built on top of an assumption that turned out to be false.

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
crates/         the database, 26 library crates plus the shell
spec/           the technical design, twenty documents, written before the code
xtask/          build automation, including the layer rule and the prose check
```

[`spec/18-package-layout.md`](spec/18-package-layout.md) explains the rank of each crate and why it is where it is. The two sibling repositories are [`tamnd/rudb-compat`](https://github.com/tamnd/rudb-compat), which is the differential harness against a real DuckDB, and [`tamnd/rudb-bench`](https://github.com/tamnd/rudb-bench), which is the benchmark harness. They are separate so that a result can be reproduced by someone who does not trust us, without building the engine from a specific commit of the engine's own repository.

## The specification

Twenty documents, written before the implementation. They are in the repository rather than a wiki because they are reviewed and revised in pull requests like everything else, and because a design document that drifts from the code is worse than no design document.

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
| [17](spec/17-milestones.md) | M0 to M11, exit criteria, and the three places it is sane to stop |
| [18](spec/18-package-layout.md) | The crate tree, dependency rules, stability tiers |
| [19](spec/19-open-questions.md) | The ranked list that has to be answered, and by when |

Read 02 first, then 03, then 01. Document 02 decides whether the project is honest, document 03 is the measurement it rests on, and document 01 is the literature that says the measurement is reachable.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). The short version is that a change to behavior comes with a test that would fail without it, a new encoded fast path comes with the equivalence test that runs it against the decoded path, and a performance claim comes with the command that reproduces it.

## Not in scope

Distributed execution, permanently. A cluster is the wrong answer to a 2026 machine with 448 cores. Not a transactional database, though it has MVCC and ACID because compatibility requires them. Not a lakehouse catalog and not a streaming system. Not a general dataframe library before 1.0. No GPU in the core, because a GPU-first engine gives up the runs-on-a-laptop property that is most of the reason anybody uses an embedded database. Each of those is argued where it belongs in the specification rather than quietly unmentioned.

## License

Apache-2.0. See [LICENSE-APACHE](LICENSE-APACHE).

Not affiliated with, endorsed by or derived from DuckDB Labs. The compatibility surface is reimplemented against the published format and the generated C header.

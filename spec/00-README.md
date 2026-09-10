# Spec 2140: rudb

An embedded analytical database written in Rust. Its own storage format, its own compression layer, its own vectorized execution engine, its own planner, its own JIT backend. Binary compatible with DuckDB on three surfaces: the on-disk file format, the extension C ABI, and the SQL dialect. The performance target is an order of magnitude past DuckDB on published benchmarks and an order of magnitude under it on resource consumption. Single node, no cluster, no external runtime.

Repo: `github.com/tamnd/rudb`. Sibling repos: `github.com/tamnd/rudb-compat` for compatibility, `github.com/tamnd/rudb-bench` for measurement. All three GitHub names and all the `rudb-*` crate names in document 18 were confirmed free on crates.io on 10 September 2026 by direct API query rather than assumed. Written 10 September 2026.

## The situation as of today

DuckDB is about to ship [v2.0 "Cyanoptera"](https://duckdb.org/2026/08/17/duckdb-20-highlights), currently in [alpha](https://duckdb.org/2026/09/02/try-duckdb-20-alpha) at `v2.0.0-alpha39998`, with general availability projected for the second half of October 2026. It is over 10,000 commits past v1.5.x and it changes four things that matter to this project. It replaces the PostgreSQL-derived parser with a PEG parser with runtime-extensible grammar. It adds a client/server mode over a wire protocol called Quack. It freezes a versioned C extension API defined by a YAML specification with explicit lifecycle and stability tags, which for the first time makes "implement DuckDB's ABI" a well posed problem rather than a chase. And it moves the storage format to v2.0 with lazy column metadata, `DICT_FSST` as a default encoding, compact delete storage, and asynchronous I/O through the whole engine.

That last set is the important one. Until v2.0, targeting DuckDB compatibility meant targeting a moving C++ API and a storage format whose version number incremented every minor release: 64 for v0.9 through v1.1, 65 for v1.2, 66 for v1.3, 67 for v1.4, 68 for v1.5. The v2.0 freeze is the first stable target this project could have aimed at, and it is arriving right now.

The performance situation is equally well defined and much less comfortable. Document 03 works through the measured numbers. The short version: on ClickBench at `c6a.4xlarge`, summing the best hot run of each of the 43 queries, DuckDB is at 26.25 seconds, ClickHouse at 18.07 seconds, and Umbra at 8.10 seconds. Ten times faster than DuckDB is 2.63 seconds, which is 3.1x past the fastest CPU engine anyone has published on that machine. That is the number this specification has to justify, and document 02 is where it either closes or does not.

## The one result that changed the design

In March 2026 Wehrstein, Eckmann, Jasny and Binnig published [Bespoke OLAP](https://arxiv.org/abs/2603.02001) (PVLDB 19(11), VLDB 2026). They used an LLM-driven synthesis pipeline to generate a C++ query engine specialized to one fixed workload and measured 11.17x over DuckDB 1.4.1 on TPC-H and 45.33x on the CEB join workload, single threaded and core pinned. Against Umbra, a compiled-query engine, they measured 7.24x and 9.56x.

The headline number is not the useful part. The ablation is. When the synthesized engine was constrained to a flat struct-of-arrays storage layout and allowed to specialize only its code, it got **1.26x on TPC-H and 0.57x on CEB**, meaning it lost. When it was allowed to specialize the storage layout too, it got **12.35x and 51.40x**.

Query compilation, operator fusion, restrict hints, manual unrolling, branchless range checks and software prefetch, applied on top of a conventional columnar layout, bought about 26 percent. Choosing a different physical representation for the data bought an order of magnitude and then some.

Every design decision in this specification follows from that inversion. Document 02 states it as the project thesis, documents 05 and 06 are the two longest documents in the series because they are where the storage layer lives, and document 07 is deliberately written as "make the execution engine fast enough to not be the bottleneck" rather than "make the execution engine the source of the win."

## The goal, stated so it can be falsified

Four axes, each with a number, each measured by the methodology in document 15. Document 02 argues each one and says plainly where it does not hold.

**Compatibility.** rudb opens, reads, writes and checkpoints DuckDB v2.0 storage format files with byte-level round trip fidelity. It passes the DuckDB SQL logic test corpus at 99% or better with every failure enumerated and categorized. It exposes `duckdb_ext_api` such that an extension binary compiled against DuckDB's stable C API loads and runs unmodified. This is measured continuously by differential execution in `rudb-compat`, never asserted.

**Aggregate performance.** 10x on total ClickBench hot runtime and on the ClickBench Combined metric against DuckDB, same instance, untuned load-and-go category. 10x on TPC-H at SF100 total runtime. 5x or better on TPC-DS and on the JOB and CEB join-order workloads, where the bound is planner robustness rather than scan throughput and where document 09 explains why 10x is a different problem.

**Per-query floor.** No query slower than DuckDB, ever, on any suite. 10x or better on every query where DuckDB is more than 3x away from the hardware bound for that query's shape. On the queries where DuckDB is already within 2x of the bound, parity plus whatever the compression layer gives for free. Document 03 identifies which ClickBench queries fall in each bucket, by measurement.

**Resource.** 10x smaller on-disk footprint than DuckDB on the same data, which on ClickBench hits means 20.46 GB down to roughly 2 GB. 10x lower peak resident set on the same query at the same thread count. 10x fewer total CPU-seconds. This axis is the one nobody publishes and it is the one that decides whether the engine is actually better or merely trading memory for time.

## Why Rust, concretely

Not for fashion, and not because "Rust is fast." Four specific reasons, in order of how much they matter here.

**The compilation tier is free.** A serious analytical engine needs a JIT for expression evaluation and pipeline fusion. In C++ that means LLVM, which the [CGO 2024 study of compiler frameworks for query compilation](https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf) measured as an order of magnitude slower to compile than a purpose-built single-pass backend. That study also measured Cranelift, and found it only 20 to 35 percent faster than LLVM and 16x slower than Umbra's own single-pass backend, plus a pile of C++ to Rust glue. In a Rust engine the glue disappears entirely, Cranelift is a normal dependency, and the fallback of writing our own single-pass backend is a bounded project rather than a rewrite. Document 08 sequences this.

**Data race freedom in a morsel-driven engine is a type system property, not a review property.** The engine's parallelism model is dozens of worker threads pulling morsels off shared queues and writing into shared partitioned hash tables. That is precisely the code where C++ engines get subtle, non-reproducible corruption, and where `Send`, `Sync` and the borrow checker turn a class of bugs into compile errors. This is worth more than it sounds because it is what makes it safe to keep rewriting the parallel layer, and the parallel layer is going to get rewritten several times.

**The ecosystem carries the boring parts.** [`arrow-rs`](https://crates.io/crates/arrow) gives the Arrow C data interface and the zero-copy FFI boundary. [`parquet`](https://crates.io/crates/parquet) gives a reader we can beat but do not have to start from. `rayon` and `crossbeam` give the work-stealing substrate. `object`, `gimli` and `cranelift` give the codegen tail. `insta`, `proptest`, `arbitrary` and `cargo-fuzz` give the test apparatus. Document 18 is explicit about which of these are load bearing and which are placeholders we intend to replace.

**Arena and index based data structures are the natural idiom.** The storage layer is a graph of encoded chunks referencing each other through cascading encodings, and the plan is a graph of operators referencing each other. Both are index-into-arena structures, which is the shape Rust is comfortable with and the shape that avoids the pointer chasing that dominates a naive implementation.

The honest counterweight: `std::simd` is [still nightly only](https://github.com/rust-lang/portable-simd/issues/364) with no stabilization date, so document 07 commits to `core::arch` intrinsics behind a runtime dispatch layer on stable Rust, with `std::simd` as a nightly-gated alternate path we measure against but do not require.

## Settled decisions

**Our own storage format, and DuckDB's format as an import and export path.** rudb's native format is described in documents 05 and 06 and is designed for the resource axis, which DuckDB's format cannot reach. DuckDB v2.0 files are a first-class attachable format, read and written losslessly, not a migration tool. A user with a 200 GB DuckDB file gets full compatibility on day one and the 10x resource win only after `CHECKPOINT INTO` rewrites it. Document 12 covers both directions.

**Physical layout is chosen at runtime and re-chosen over time, not fixed at write time.** This is the project thesis and it is where the order of magnitude comes from. Each column has a physical representation drawn from a small closed set, selected by a sampling-based encoder at write time and revised by a background recompressor based on observed query behavior. Documents 05 and 06.

**Execution runs on encoded data wherever the encoding permits it.** A dictionary encoded column is grouped on its codes. A frame-of-reference column is filtered by transforming the predicate rather than the data. A run-length encoded column is aggregated by arithmetic on the runs. Decoding is a fallback path, not the default path. Document 07.

**Four execution tiers, chosen adaptively per pipeline.** Interpreted vectorized for short queries, a fused vectorized path for the common operator chains, Cranelift-compiled for long-running pipelines, and a hand-written single-pass backend if and only if document 08's M6 experiment shows Cranelift's compile latency is the binding constraint. Umbra's [adaptive execution](https://db.in.tum.de/~freitag/papers/p29-neumann-cidr20.pdf) and Photon's [per-batch adaptivity](https://people.eecs.berkeley.edu/~matei/papers/2022/sigmod_photon.pdf) are both prior art and both are cited in document 08 for what they force.

**Robust Predicate Transfer is in the planner from M4, not bolted on later.** Zhao, Su, Yang, Yu, Koutris and Zhang's [SIGMOD 2025 result](https://arxiv.org/abs/2502.15181) showed that with RPT, join order stops being the dominant risk for acyclic queries, with orders of magnitude better robustness and 1.5x geometric mean end to end when integrated into DuckDB. An engine that gets join ordering right by being robust rather than by having a better cardinality estimator is a fundamentally more defensible design, and it is the only credible path to the JOB and CEB numbers in axis 2. Document 09.

**Vector size is a compile-time constant of 1024, not DuckDB's 2048.** 1024 is the [FastLanes](https://www.vldb.org/pvldb/vol18/p4629-afroozeh.pdf) unit and matching it means the compression layer and the execution layer share a granularity, which is what makes compressed execution cheap rather than awkward. The DuckDB compatibility layer converts at the boundary. Document 07 justifies this and document 12 covers the conversion cost.

**No GPU in the core.** Sirius, the GPU engine that plugs into DuckDB over Substrait, holds the ClickBench cost-efficiency record on a GH200 with [roughly 7x better cost efficiency than the top CPU systems](https://developer.nvidia.com/blog/nvidia-gpu-accelerated-sirius-achieves-record-setting-clickbench-record/). That result is taken seriously in document 01. It is still not what we build, because a GPU-first engine gives up the embedded, dependency-free, runs-on-a-laptop property that is most of the reason DuckDB is used at all. An offload crate stays possible and stays optional.

**No distributed execution.** rudb speaks the Quack wire protocol for compatibility with DuckDB 2.0 clients and it does not shard. The [Cloudspecs analysis](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/cloudspecs-final.pdf) (Steinert, Kuschewski and Leis, CIDR 2026) is the argument: over 2015 to 2025 maximum instance core count grew by an order of magnitude to 448 cores on `u7in`, network bandwidth per dollar improved by an order of magnitude, but per-core CPU and DRAM gains were much smaller and NVMe performance per dollar has been flat since 2016. The 2026 answer to a large analytical workload is one very large machine.

**DuckDB's PEG grammar is vendored verbatim rather than transcribed.** In v2.0 DuckDB replaced its bison parser with a PEG parser whose grammar ships as sixty one kilobytes of declarative text with no semantic actions in it, MIT licensed. That text is the definition of the dialect we claim compatibility with, so we take the file rather than retyping 1,086 rules and then retyping their diff on every release. A generator turns it into a Rust rule table, the matcher interprets that table, and `cargo xtask grammar` fails the build if anybody edits the vendored tree. The tokenizer, the transformer and the binder are still ours, and document 20 is explicit about which parts of compatibility this buys and which three it does not.

**Every layer has a textual form and a round-trip parser.** Logical plan, physical plan, encoded chunk metadata, the compiled pipeline IR. Every stage can be dumped, diffed, fuzzed and bisected in isolation. This is what "modular" cashes out to, and it is the mechanism by which a new result from the literature can be dropped into one crate and measured without touching the others. Documents 04 and 16.

## The documents

| | | |
|---|---|---|
| 00 | this file | the pitch, the settled decisions, what to read first |
| 01 | `01-research-2026.md` | the verified landscape as of September 2026, and what each result forces |
| 02 | `02-the-goal.md` | the four axes, what is reachable on each, and the honest version of 10x |
| 03 | `03-baselines.md` | measured ClickBench and TPC-H numbers, where DuckDB's time actually goes |
| 04 | `04-architecture.md` | layers, dataflow, threading model, the error and cancellation model |
| 05 | `05-storage.md` | the file format, block layout, zone maps, the buffer manager, I/O |
| 06 | `06-compression.md` | the encoding set, cascading, multi-column compression, compressed execution |
| 07 | `07-execution.md` | vectors, operators, morsels, hash tables, strings, spilling |
| 08 | `08-codegen.md` | the four tiers, expression IR, Cranelift, the single-pass fallback |
| 09 | `09-optimizer.md` | logical rewrites, RPT, cardinality, physical layout adaptation |
| 10 | `10-sql-and-types.md` | the type system, the DuckDB dialect surface, functions, nested types |
| 11 | `11-transactions.md` | MVCC, WAL, checkpointing, the single-writer model, DDL |
| 12 | `12-duckdb-compat.md` | storage format, C ABI, extensions, Quack protocol, and the compat tiers |
| 13 | `13-ecosystem.md` | Parquet, Arrow, Iceberg, DuckLake, the extension model, language clients |
| 14 | `14-rudb-compat.md` | the differential harness, the corpora, how a compat claim is earned |
| 15 | `15-rudb-bench.md` | what we measure, against whom, and the rules for reporting it |
| 16 | `16-testing.md` | unit, property, fuzz, crash consistency, and the correctness apparatus |
| 17 | `17-milestones.md` | M0 to M11, exit criteria, and the three places it is sane to stop |
| 18 | `18-package-layout.md` | the crate tree, dependency rules, stability tiers |
| 19 | `19-open-questions.md` | the ranked list that has to be answered, and by when |
| 20 | `20-the-grammar.md` | why DuckDB's PEG grammar is vendored, what it does and does not buy |

Read 02 first, then 03, then 01. Document 02 decides whether the project is honest, document 03 is the measurement it rests on, and document 01 is the literature that says the measurement is reachable.

## What this is not

Not a transactional database. rudb has MVCC and ACID because DuckDB does and compatibility requires it, but it is tuned for one writer and many readers over columnar data, and document 11 is explicit that OLTP throughput is not an axis.

Not a distributed query engine, not a lakehouse catalog, and not a streaming system. It reads Iceberg and DuckLake tables because DuckDB does, per document 13, and it does not manage them.

Not a fork of DuckDB and not a wrapper over DataFusion. The compatibility surface is reimplemented against the published format and the generated C header, which is exactly why `rudb-compat` has to exist and why document 14 is as long as it is.

Not a general dataframe library. There is a Polars-shaped API someone will eventually want and it is not in scope before 1.0.

## Honesty about scope

Somewhere between 60 and 100 engineer-months to 1.0 as specified, which is five to eight person-years. The compatibility surface is most of the risk in the estimate, not the engine. An engine that runs ClickBench fast is maybe 25 of those months. An engine that also passes the DuckDB SQL logic tests, reads DuckDB files, loads DuckDB extensions, and handles the long tail of `LIST`, `STRUCT`, `MAP`, `UNION`, `VARIANT`, lambdas, `PIVOT`, `ASOF JOIN`, recursive CTEs with `USING KEY`, and the roughly 1,500 built-in functions is the other 50 to 75.

The two things that actually kill projects like this:

**The compatibility surface has no bottom.** DuckDB's dialect is deliberately large and growing, v2.0 alone adds triggers, `NEAREST` joins, DML inside CTEs, nested schemas, and a shredded end-to-end `VARIANT` type. Chasing it feature by feature is a losing race against a well funded team. The defence is document 14's inversion: we do not implement features from a list, we run their test corpus and their extension binaries against us continuously from M2 and let their tests define the list.

**The resource axis is the one that will not close by grinding.** The other three axes respond to effort. Getting ClickBench hits from 20.46 GB to 2 GB does not, because it requires the multi-column compression in document 06 to actually find and exploit the correlations in that dataset, and if the correlations are not there the number is not reachable at any amount of engineering. Document 19 makes this open question one and document 17 puts the experiment in M1, before anything depends on it, precisely because it is the assumption most likely to be false.

The riskiest technical assumption is that a general purpose engine can capture most of Bespoke OLAP's layout specialization win through runtime adaptation. Their result is for a fixed, known workload with unlimited synthesis time. Ours has to decide on the fly, from samples, with a background thread and a budget. If runtime adaptation captures only a third of the win the aggregate axis becomes 4x rather than 10x, which is a good database and not the one specified. Document 19 open question two, experiment in M3.

## On the name

`rudb` is Rust plus DB. Four letters, unambiguous, types quickly, and it sits in the naming line that runs `duckdb`, `chdb`, `slatedb`, `bemidb`, `gendb`. It does not pun on ducks, which is deliberate: a compatible reimplementation that also imitates the branding invites a trademark conversation nobody wants, and DuckDB Labs owns the duck.

Checked directly on 10 September 2026 rather than assumed: `crates.io/crates/rudb` returns "does not exist" and is free, as are `rudb-core`, `rudb-storage`, `rudb-exec`, `rudb-compat` and `rudb-bench` and every other name in document 18. `github.com/tamnd/rudb`, `tamnd/rudb-compat` and `tamnd/rudb-bench` all return 404 and are free.

There is an unrelated dormant `rudb` on GitHub from a Redis clone experiment and it holds no registry namespace. The binary name is `rudb`, and the CLI is deliberately argument-compatible with the `duckdb` CLI so that scripts work, which document 13 covers.

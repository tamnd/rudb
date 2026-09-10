# Measurement

This document is not a layer. It is the machinery every layer's gate depends on, and it is written separately because the gates in documents 03 through 12 all assume numbers that nothing currently produces.

The rule from document 00 is that a layer is done when it is measured on real data against DuckDB and ClickHouse and the number is better. That rule is only enforceable if the measuring is real, so this is what has to be true of `rudb-bench` and `rudb-compat` for the rest of the directory to mean anything.

## 13.1 What exists in `rudb-bench`

2196 lines across nine files and the shape is good.

`Engine` is a trait with `name`, `version`, `can_run`, `load` and `run`. `can_run` is there because rudb cannot run a query and a harness that discovered that by timing a failure would report a very good number for a query that did nothing. `DuckDB` is implemented as a subprocess driven through its binary. `Rudb` is a unit struct that declares it cannot run.

`Suite` describes `smoke`, `clickbench` and `tpch`, each with what it needs, whether it is comparable to a public board, and a note saying what it is and is not for. The notes are the good part: `smoke` says any figure from it is labelled smoke and goes nowhere near a README, and `clickbench` says it is run to the official rules so the number is comparable and is reported as both the official combined metric and the raw sum because the sum is diagnostic and the combined is comparable.

`Distribution` holds a full sample rather than a single number, with a ClickBench convention constructor and a median constructor, and it exposes the quartiles, the interquartile range, the relative interquartile range and a `publishable` predicate that refuses a number too noisy to publish. `Runs::collect` drives the cold and hot runs.

`memory.rs` samples peak resident set. `fleet.rs` has `Role::{Reporting, Regression, Correctness}` with `may_publish` returning false for every machine the project owns.

That is a harness that has been thought about. What follows is what it does not have.

## 13.2 The rudb engine

`Rudb` has to become a real engine and the decision that matters is in-process or subprocess.

It is a subprocess, driven through the `rudb` CLI binary exactly as DuckDB is driven through its binary. The reason is fairness: `engine.rs` already runs a fresh process per query and already documents the consequence, which is that hot means page cache warm rather than buffer pool warm. Running rudb in-process while running DuckDB as a subprocess would give rudb a free process start, a warm allocator and a warm buffer pool on every hot run, and it would produce a flattering number that is not comparable with anything. A harness whose own engine is a special case is a harness that gets edited on the day it produces a bad number, which section 2.1 of document 02 already said.

The consequence is that `rudb-cli` has to be good enough to be benchmarked through: it has to read a query from an argument, execute it, and exit without printing a hundred million rows to a pipe. DuckDB's ClickBench scripts handle this by sending output to null, and rudb does the same, with the important caveat that an engine which optimizes away work because the output is discarded is cheating. Results are fully materialized and then dropped, and there is a test asserting that the row count is what it should be, run separately from the timing.

`can_run` stops being a constant and becomes per suite, because after 2b rudb can run `smoke` and cannot run `clickbench`, and after 2d it can run both. That is the honest representation of the staged state document 05 section 5.2 describes, and it means the report says unable to run rather than showing a blank or a zero.

## 13.3 The other three engines

**ClickHouse** as two rows, per document 02 section 2.2: `clickhouse local` for the load-and-go category and a real server with the official ClickBench `create.sql` and its sorting key for the tuned category. Both are subprocess-driven. The server one needs a start and stop discipline and a check that the server is warm before the cold run, which is a contradiction that has to be resolved explicitly: the cold run means the page cache is dropped, and dropping the page cache under a running ClickHouse server means the server's own caches are also cold, which is the right comparison as long as it is stated.

**DataFusion** through its CLI binary, which is the fairest available control for whether a difference is Rust or is rudb.

**Polars** through a small Python driver in `sink` mode, only on suites where a dataframe expression is a faithful translation of the SQL, and mainly for the out-of-core comparison in document 10 section 10.11.

All four are pinned by exact version, and the version string appears next to every number, which the `Engine` trait already requires.

## 13.4 CPU seconds

Document 02 section 2.5 says CPU seconds is the axis that decides whether this project is real, because wall clock ratios can be bought with threads and a ten times wall clock win at eight times the CPU is a scheduling result rather than an engine result. Nothing currently measures it.

For a subprocess it is user plus system time across every thread of the child, which the operating system reports at wait time. On Linux and macOS that is `getrusage` with `RUSAGE_CHILDREN`, or equivalently the fields `wait4` fills in, declared as an `extern "C"` binding in `rudb-bench` since it is a small and stable interface. On Windows it is `GetProcessTimes` against the child handle.

The cross-check is `/usr/bin/time`, which reports the same numbers from a different source, run once per suite rather than per query. Two independent sources agreeing is what makes a number trustworthy, and a mismatch is a bug in the harness rather than in the engine.

Peak resident set is already sampled by `memory.rs`, and `getrusage` also reports a maximum resident set for the child, so the same cross-check applies there and the existing sampler becomes the higher-resolution source rather than the only one.

## 13.5 Bytes read

Also required by document 02 section 2.5 and also unimplemented, and it is the measurement that catches a pruning bug that would otherwise be invisible, per document 05 section 5.10.

For rudb the number comes from inside, counted in `rudb-io` where every read passes through one trait, which is the interception shim section 16.5 of the parent spec describes. That is exact and it costs nothing.

For the other engines there is no shim, so it comes from the operating system: on Linux, the `read_bytes` field of `/proc/<pid>/io` for the child, sampled just before the child exits. That counts bytes actually fetched from block devices rather than bytes requested, which is the right quantity for a cold run and is not the right quantity for a hot one, where the page cache serves everything and the number is near zero. So bytes read is reported for cold runs only and the report says so.

Comparing rudb's internal count against its own `/proc` count on a cold run is another two-source cross-check and it validates the shim.

## 13.6 The microbenchmarks

Every layer document specifies a microbenchmark group: `kernels` in document 03, `expressions` in document 04, page decode and pruning in document 05, hash build and probe in document 06, grouped aggregation in document 07, join probe in document 08, sort and top-N and window in document 09, scaling curves in document 10, planning time and q-error in document 11, and the worst-case table in document 12.

They all live in `rudb-bench` rather than as separate benchmark harnesses inside `rudb`, for one reason: the ledger in document 02 section 2.8 is the artefact that says what each layer bought, and a number that is not in the ledger is a number that is not in the argument. Two benchmark systems means two formats, two machines-of-record and two sets of results that cannot be put in one table.

That means `rudb-bench` gains a dependency on `rudb` as a library, alongside driving it as a subprocess for whole-query work. Those are two different uses and they coexist: whole-query numbers are subprocess numbers for fairness, and microbenchmark numbers are in-process because a microbenchmark of a comparison kernel has no meaningful process to start.

The microbenchmark driver has to handle the things a microbenchmark harness handles, and since there is no `criterion` under the dependency rules, they are written: a warmup, a run count chosen so the measured interval is long enough to be above timer noise, the full distribution rather than a mean, and outlier reporting. `measure.rs` already has most of this and it generalizes.

## 13.7 The ledger

Document 02 section 2.8 specifies it: one row per layer, with the commit range, and for each suite the before and after on total time, CPU seconds, peak resident and bytes read, on the same machine, with comparison engine versions held fixed or restated.

It is generated from the stored runs rather than written by hand, the stored runs are committed, and the release notes are written from it. A release note that says the hash table made joins faster and cannot point at a row is a release note that is guessing.

The generation is a command, the output is a markdown file in `rudb-bench`, and it is linked from all three READMEs.

## 13.8 The regression gate

`smoke` runs on every commit in CI, and it fails the build when a query regresses past a threshold rather than printing a number nobody reads. That is one of the three repeatability requirements in document 02 section 2.9.

The threshold has to account for noise on a shared CI runner, which is considerable. `Distribution::publishable` and `relative_iqr` already exist and are the right mechanism: a run too noisy to be publishable cannot fail the build either, because a gate that fires randomly is a gate that gets disabled. So the rule is that a regression fails the build when the distributions do not overlap and the median moved past the threshold, and a noisy run is reported and not failed.

The real regression detection happens on `server3`, on a schedule, where the machine is not shared and the numbers are comparable across weeks. CI catches a factor, the scheduled run catches a percentage.

`cargo xtask bench` runs the whole thing from one command on a machine that has the data, which is the second repeatability requirement.

## 13.9 What `rudb-compat` owes

The corpus pass rate is published on every run and it appears next to every performance number, because document 00's third consequence is that conformance does not stop while the layers are built: the rate is allowed to move slowly during a performance layer, it is not allowed to fall, and a wrong answer takes priority over every performance item in this directory.

Two things have to be added for that to work with this plan.

Per-layer regression detection, meaning that when a layer's rewrite lands, the corpus is run with the old and new code paths and the diff is examined rather than only the total being compared. A rewrite that fixes one hundred records and breaks one hundred others shows no movement in the rate and is a disaster.

Timing in the corpus runner. The four files that timed out at ten seconds were the measurement that redirected this whole plan, and they were found by accident. Recording per-file execution time turns that accident into a signal, and a file whose time changes by a large factor is worth looking at whether it got slower or faster.

## 13.10 The reporting machine

Document 02 section 2.7 proposes renting a `c6a.4xlarge` quarterly and at every minor release, because none of the fleet is comparable to a published board row and `Role::may_publish` structurally prevents publishing from a regression machine.

That stands, with one addition from document 10 section 10.4: the fleet has no NUMA machine, so the NUMA hook in the scheduler is untestable on owned hardware. A rented instance with more than one socket would make it measurable, and that is a second reason to rent beyond publishing.

## 13.11 Where this work happens

This document is not a sub-milestone. Its pieces are spread across the others, and each one lands with the layer that needs it, which is the only ordering that keeps the harness honest, because a harness feature built ahead of the number it will report is a harness feature built without knowing what it needs to say.

The rudb engine, ClickHouse, DataFusion, Polars, CPU seconds and the first ledger row land at 2a. The microbenchmark driver and the `kernels` group land at 2b. Bytes read and the shim validation land at 2d with the first file scan. `cargo xtask bench` and the CI regression gate land at 2a because they are repeatability rather than measurement. The corpus timing and per-layer diff land at 2b, because 2b is the first layer that rewrites code the corpus covers heavily.

# The baseline, which comes before everything else

Nothing in this directory can be evaluated without a number from before it. This document specifies the number from before it, and it is sub-milestone 2a, ahead of layer one.

The rule from document 00 was that a layer is done when it is measured against DuckDB and ClickHouse on real data and the number is better. That rule has a precondition nobody can skip: there has to be a first run where every engine including rudb is measured on the same data on the same machine with the same method, and where the result is written down whatever it says. That run is the baseline. Every later claim in this directory is a delta against it, and a delta against a number that was never recorded is not a delta.

## 2.1 What the baseline is for, and what it is not for

It is for attribution. When the hash table lands and TPC-H Q9 gets faster, the question that has to be answerable is how much of that came from the hash table and how much came from the four other things that were merged that month. The only way to answer it is to have a number per query per engine per commit, going back to the beginning, on a machine that has not changed.

It is not for publicity and it is not a leaderboard entry. None of the machines this project owns is a `c6a.4xlarge`, which is the machine the public ClickBench board uses, so nothing measured on them is comparable to a published row and the harness already enforces that structurally through `Role::Reporting` in `src/fleet.rs`. The baseline is a regression number. Section 2.7 says what to do about the reporting number.

It is also not a claim that rudb is fast. On the first run rudb will lose almost everything, and the report has to say so in the same table, in the same units, without a footnote explaining the loss away. A harness whose own engine is a special case is a harness that gets edited on the day it produces a bad number.

## 2.2 The engines

Five, in the order they get wired up.

**DuckDB** is already in `src/engine.rs` and is the primary comparison, because the compatibility claim is against it and because it is the closest thing to rudb in kind: embedded, single node, no server, columnar, vectorized. Pinned at v2.0 once it releases, run from a downloaded release binary rather than a build, with the exact version string in every row.

**rudb** is in `src/engine.rs` as a stub that declares it cannot run a query. Turning that stub into a real engine is the first task of 2a and it is the task that makes the whole directory possible.

That task turned out to be smaller than it should be, for a reason found while writing document [05](05-scan.md) and recorded there in section 5.2: rudb has no path from a file on disk into a chunk at all. Every table is a `MemoryTable` filled by `INSERT`, `rudb-parquet` is a nine line stub, and the M1 encoder is a standalone laboratory connected to neither end of the engine. So at 2a rudb runs the `smoke` suite and TPC-H SF1 loaded through `INSERT`, and on `hits` and on SF100 it is recorded as unable to run rather than as a slow number. Section 2.6 is corrected accordingly, and the first real rudb number on `hits` comes from sub-milestone 2d.

**ClickHouse** is next and is the harder of the two to be fair to. It is not embedded, it wants a server, and its storage engine wants the data loaded in its own format with a sorting key chosen. The published ClickBench entry for ClickHouse chooses a sorting key, which is part of why it wins, and a run that does not choose one is measuring a differently configured system. The harness runs `clickhouse local` for the load-and-go category and a real server with the official ClickBench `create.sql` for the tuned category, and reports them as two rows rather than one, because they are two systems.

**DataFusion** fourth, because it is the other serious Rust engine, because it held the ClickBench Parquet leaderboard, and because it is the fairest available answer to the question of whether a difference is Rust or is rudb.

**Polars** fifth, in `sink` mode against the rewritten streaming engine, and only on the suites where a dataframe expression is a faithful translation of the SQL. It is included for the out-of-core comparison in document 10 more than for the scan comparison.

Every engine runs in a fresh process per query, which `src/engine.rs` already does and already explains. The consequence, that hot means page cache warm rather than buffer pool warm, is weaker than the ClickBench convention and is stated in every report.

## 2.3 The data

Three datasets at 2a, all of which already exist or are being generated.

**ClickBench `hits`**, 99,997,497 rows and 105 columns, on `server1` and `server3` at `~/rudb-data/hits.parquet`. This is the dataset the headline claim is stated against and the one M1 measured the format on. 13.76 GB as Snappy Parquet, 20.46 GB in DuckDB's format, 9.65 GB in rudb's.

**TPC-H SF100**, generating on `server1` now, a hundred `dbgen` partitions combined per table into single Parquet files with a 122,880 row group size. SF100 rather than SF10 because SF10 fits in cache on every machine in the fleet and therefore measures the cache.

**A subset for the laptop.** ClickBench `hits` first five million rows, and TPC-H SF1. Neither number is ever published. They exist so that a person changing the hash table can get a signal in ninety seconds without ssh, which is the difference between a benchmark that is run on every branch and one that is run before a release.

TPC-DS, JOB and CEB come in at sub-milestone 2k with the optimizer, because they measure planning and there is no planning at all yet. Document [11](11-optimizer.md) section 11.9 is what they are for.

## 2.4 The machines

The fleet, with what each one is for. The roles are the ones already in `src/fleet.rs`.

| machine | cores | RAM | disk | role |
|---|---|---|---|---|
| server3 | 8 | 23 GB | 33 GB free | Regression, primary |
| server1 | 4 | 5 GB | 91 GB free | Regression, and the only one with room for SF100 |
| server2 | 6 | 11 GB | | Correctness, it is too noisy to time on |
| gamingpc | Windows | | | Correctness, and the only Windows target |
| laptop | 10 | | | Correctness, and the fast loop |

The split between `server1` and `server3` is awkward and worth naming: `server3` has the cores and the memory and is the machine to time on, and `server1` has the disk and is the only machine the SF100 dataset fits on. Until that is resolved, TPC-H SF100 numbers come from `server1` at four cores, and every TPC-H row carries that. The resolution is more disk on `server3`, and it is a purchase rather than a design decision.

`gamingpc` is not decoration. A single node embedded database that is only tested on Unix will have a Windows path bug in the I/O layer, and the async I/O work in document 05 is exactly the kind of work that has one. It runs the correctness suites and the smoke benchmark on every release, and it never produces a timing anybody quotes.

## 2.5 What gets measured

Per query, per engine, per machine, per commit.

Cold wall clock, meaning the first run after the page cache is dropped. Hot wall clock, best of five and the full distribution rather than the best alone, because a mean that hides a bimodal distribution hides the thing worth knowing. Peak resident set, from the sampling in `src/memory.rs`. Total CPU seconds, which is user plus system across every thread, because wall clock on eight cores hides an engine that is burning all of them to do the work of two. Bytes read from the file system, through the interception shim in document 16.5 of the parent spec, because a scan that is fast because it read the wrong amount of data is a scan whose number will move when the data grows.

Per dataset, per engine: load time, and on-disk size after load.

The four axes in document 02 of the parent spec are runtime, per-query floor, resource and compatibility, and the list above covers the first three. Compatibility is `rudb-compat` and is reported next to them rather than inside them.

**CPU seconds is the axis that decides whether this project is real.** Wall clock ratios can be bought with threads, and a ten times wall clock win at eight times the CPU is a scheduling result, not an engine result. The parent spec's resource axis asks for ten times fewer CPU seconds and that is the number that is hardest to fake.

## 2.6 What the first table is expected to say

Writing the expected result down before running it is how a benchmark stays honest, so here is the prediction, and the report will either match it or the mismatch is itself a finding.

rudb wins on-disk size on `hits`, at 9.65 GB against DuckDB's 20.46, which is 0.47. That is the one column where rudb is already ahead of every engine in the table, it is the M1 result, and it is the reason the baseline is worth publishing at all rather than being embarrassing.

rudb loses load time badly. Encode is 5 MB/s of values a core after front coding, which is 5.6 CPU hours for `hits`, and DuckDB loads it in minutes. The write path being twenty times slower than the read path is already recorded in the changelog as not shippable.

rudb loses every query it can run, and loses the joins by a lot, because the join is a nested loop and no filter is pushed down. The four corpus files that timed out at ten seconds are the evidence. Expect two to three orders of magnitude on anything with a join and one order on scan-and-aggregate. On `hits` and on SF100 it does not lose, it abstains, for the reason in section 2.2.

rudb probably wins peak resident on the scan queries, because the encoder streams and the M1 run held 105 columns of a 14 GB file under a gigabyte, and probably loses it on anything that builds a hash table, because there is no memory manager at all.

A table that says all of that, published, is the correct output of 2a. The point of writing the prediction here is that if rudb turns out to win a query, that is a surprise, and a surprise in a benchmark is usually a bug in the benchmark.

## 2.7 The reporting machine problem

Regression numbers come from the fleet. Published numbers cannot, because the fleet has no `c6a.4xlarge` and the whole point of that machine is that the public board uses it.

The answer is to rent one. A `c6a.4xlarge` is sixteen vCPU and 32 GB, and a full ClickBench run on it is a small number of hours. The proposal is a quarterly published run, and one at every minor release, on a freshly launched instance with the official scripts, with the instance id and the launch time in the report. Between those, the fleet numbers are the ones that drive decisions and they are labelled as regression numbers.

That split has to be built into the report rather than remembered, and it already is: `Role::may_publish` returns false for every machine the project owns, so a published claim from a regression machine cannot be produced by accident.

## 2.8 The attribution ledger

The output of the baseline is not a table, it is a series. Each layer in this directory closes with a row in a ledger, and the ledger is the document that says what each layer bought.

Each row is the layer name, the commit range, and for each suite the before and after on total time, on CPU seconds, on peak resident and on bytes read, on the same machine, with the engine versions of the comparison engines held fixed for the duration of the layer or restated if they moved.

The ledger lives in `rudb-bench` as generated output rather than as prose, because a hand maintained ledger is a ledger that acquires a good number that nobody can reproduce. It is regenerated from the stored runs and the stored runs are committed.

The ledger is also what the release notes are written from. A release note that says the hash table made joins faster and cannot point at a row is a release note that is guessing.

## 2.9 Exit criterion for 2a

**A committed run of `smoke`, `clickbench` and `tpch` on `server3` and `server1`, against rudb, DuckDB and ClickHouse, with every measure in section 2.5, published in `rudb-bench` and linked from all three READMEs.**

Plus three things that make the run repeatable rather than a one off. `cargo xtask bench` runs the whole thing from one command on a machine that has the data. CI runs `smoke` on every commit and fails the build on a regression past a stated threshold rather than printing a number nobody reads. And the ledger has its first row, which is the row that says what the state was before any of this started.

The sub-milestone is not done when rudb wins something. It is done when the table is complete and honest, including the losses, and when the next person can reproduce it with one command.

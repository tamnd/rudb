# Milestones

Twelve milestones, M0 through M11. Each has an exit criterion that is a measurement rather than a feeling, and each is a place where the project can be evaluated honestly. Three of them are places where stopping produces something useful, and those are marked, because a plan that only pays off at the end is a plan that has no evidence until it is too late to act on.

The ordering is driven by one principle: **the falsifiable parts come first**. Document 02.7 names four gates and document 19 names six open questions, and the schedule is arranged so that the ones that could kill the project are answered by M5 rather than by M9. Building the easy, certain parts first and discovering at month thirty that the thesis was wrong is the failure mode this ordering exists to avoid.

Effort figures are engineer-months and they are estimates by someone who has not built this. Document 00's total of 60 to 100 is the sum with the usual multiplier applied.

## M0: Skeleton

Workspace, crate tree per document 18, CI, the benchmark harness skeleton, the differential harness skeleton, the I/O interception shim from document 16.5. A parser that handles `SELECT`, a binder, an interpreter, an in-memory table, `SELECT * FROM t WHERE x > 5`.

**Exit: `cargo test` runs, `cargo xtask bench` runs and produces a table, and a trivial query executes end to end.**

The point of M0 is that the apparatus exists before the thing it measures. The I/O shim in particular is here rather than at M6 for the reason document 16.5 gives.

*Roughly 3 months.*

## M1: The format experiment

**This milestone is a measurement, not a feature.** Before building a storage engine around multi-column compression and global dictionaries, find out whether they deliver on real data.

Write a standalone encoder that ingests ClickBench `hits`, TPC-H SF100 and a handful of real Parquet datasets, and measure: distinct counts of every column, pairwise correlation and functional dependency between columns, the compressed size under single-column cascades, the additional saving from shared dictionaries and shared symbol tables, the additional saving from recomputation rules, and the encode and decode throughput of each.

**Exit: a written report with the numbers, and a decision.** The target is `hits` under 4 GB with this encoder. If it lands under 3 GB, the axis-4 claim in document 02.6 is on track. If it lands at 6 GB or above, the resource claim is wrong and documents 00, 02 and 03 are amended before another line of storage code is written.

This directly answers document 19 open questions one, four and five, and it is deliberately the first substantial thing that happens.

*Roughly 3 months.*

## M2: A working database

Full parser and binder for the core SQL surface. Catalog, transactions, MVCC, WAL, checkpointing. The native storage format with the encoding set from document 06 as validated by M1. Vectorized interpreted execution for scans, filters, projections, hash joins, hash aggregates and sorts. Parquet read. The Rust API. A shell.

DuckDB `sqllogictest` corpus running with a published pass rate.

**Exit: TPC-H SF10 runs correctly and completely. ClickBench runs correctly and completely. Neither is fast yet and that is fine. Published `sqllogictest` pass rate above 60 percent.**

*Roughly 12 months. This is the largest single milestone and it is mostly unglamorous.*

## M3: Encoded execution

The specialization contract from document 6.7 implemented: the kernel generator, dictionary codes as group and join keys, predicate transformation into the encoded domain, RLE run arithmetic, constant propagation, FSST compressed-domain substring search. Physical layout adaptation in the planner per document 9.6. The equivalence testing from document 16.2.

**Exit, and this is the project's central checkpoint: on ClickBench, at least 70 percent of processed vectors take an encoded path rather than a decoded one, and total hot time is at or below 9 seconds, which is 2.9x over DuckDB and approximately Umbra parity.**

This is the earliest point at which document 02's thesis is falsifiable. If encoded execution is implemented and ClickBench sits at 15 seconds, then layout specialization at query time is not capturing what the offline result suggested, document 19 open question two is answered negatively, and the honest response is to restate the target as 3 to 4x and say so publicly.

**This is the first sane stopping point.** A correct DuckDB-compatible engine at Umbra-class performance with a much smaller footprint is a genuinely good product even if nothing after this ships.

*Roughly 9 months.*

## M4: Multi-column compression and predicate transfer

Global dictionaries with cross-row-group codes and per-chunk code widths. Shared dictionaries and shared symbol tables. Recomputation rules if M1 said they pay. Background recompaction that upgrades old row groups to newer dictionaries.

Robust Predicate Transfer with LargestRoot and SafeSubjoin, per document 9.5. Bloom filter pushdown to scans. Cost-based join ordering with DPhyp.

**Exit: `hits` on disk at or below 3 GB. TPC-H SF100 within 2x of Umbra. JOB maximum per-query ratio against DuckDB above 5x with no query slower than DuckDB.**

The last clause matters more than the others. RPT's value is variance reduction, and a mechanism that makes eighty queries faster and three catastrophically slower has not delivered it.

*Roughly 7 months.*

## M5: The named problems

The three problems document 02.4 says the last factor of two depends on.

Heavy-hitter top-k aggregation with exact verification, per document 7.5. The global partitioned hash table with the runtime switch. The regex engine with compiled DFA, literal prefiltering and compressed-domain matching, per document 10.5. Radix sort on dictionary codes. Top-N without sorting.

**Exit: ClickBench queries 28, 32, 33, 34, 18, 16 and 13 each at or below Umbra's published time, and total hot time at or below 4 seconds, which is 6.6x over DuckDB.**

If the heavy-hitter mechanism's verification pass fails on most real distributions, document 19 open question six is answered negatively and the target lands nearer 5 seconds. That is still 5.2x and it is still a very good database. It is not 10x and the claim would have to change.

*Roughly 6 months.*

## M6: Robustness

Spilling for hash aggregate, hash join, sort and distinct. Admission control. The full crash consistency apparatus from document 16.5 with exhaustive failure point enumeration. Allocation failure testing. Concurrency model checking.

**Exit: TPC-H SF1000 completes on a 8 GiB machine. The crash simulator finds no consistency violation across an exhaustive enumeration over the standard workloads. The allocation failure test passes at every injection point.**

**This is the second sane stopping point**, and it is the first point at which the software should be recommended to anyone for data they care about. Everything before this is fast and everything before this can lose data under conditions nobody tested.

*Roughly 5 months.*

## M7: Compatibility depth

The C API with generated conformance tests from DuckDB's ABI YAML. DuckDB storage format read and write. `ATTACH` of a DuckDB file. The extension ABI host side with the published per-extension status table. The long tail of functions driven by document 10.7's weighted coverage. `VARIANT`. Triggers. `NEAREST` and `ASOF` joins. The Python binding.

**Exit: compatibility levels 0, 1 and 2 from document 12.8 published, with weighted function coverage above 97 percent and `sqllogictest` pass rate above 95 percent. A C program built against `duckdb.h` links and runs against `librudb`.**

*Roughly 10 months, and this estimate is the least reliable one in the document, for the reason document 12.7 gives.*

## M8: Compilation

The expression IR, tier 1 fusion, tier 2 Cranelift with asynchronous compilation and a code cache, perf JIT registration, tier equivalence testing per document 16.3.

**Exit: TPC-H SF100 improves by at least 15 percent over M5's numbers on compute-heavy queries with no regression on scan-heavy ones. Compile latency does not appear in the critical path of any query shorter than 200 milliseconds.**

Tier 3, the single-pass emitter, is explicitly not in this milestone and is built only if the second exit criterion fails, per document 8.1.

*Roughly 5 months.*

## M9: Operational maturity

Incremental checkpointing. Online vacuum. Better statistics including histograms and sampling for correlated predicates. NUMA-aware scheduling on large machines. Iceberg and Delta read. Object storage with caching. ADBC.

**Exit: no operation blocks writes for more than 100 milliseconds on a 100 GB database. Scaling efficiency above 70 percent from 16 to 128 threads on ClickBench.**

*Roughly 6 months.*

## M10: 1.0

The wire protocol. The remaining language bindings. Documentation. Six months of continuous fuzzing with no new wrong-answer bug, per document 16.9. Stability guarantees per document 18's tiers.

**Exit: the four compatibility levels published and stable. All benchmark suites published with full per-query tables. No known wrong-answer bug.**

**This is the third sane stopping point and the only one that is a product rather than a milestone.**

*Roughly 5 months.*

## M11: Past 1.0

Whatever the measurements say. Candidates, unranked: tier 3 if M8's latency criterion failed; GPU via Substrait per document 13.6; worst-case optimal joins for cyclic queries; learned or sketch-based cardinality estimation; workload-driven physical reorganization, which is the closest a general system can get to what Bespoke OLAP does offline; write support for Iceberg; a server deployment mode.

**No exit criterion, because this is not a plan, it is a list.**

## The three stopping points, restated

**M3.** A correct, compatible, Umbra-class engine with a smaller footprint. Worth shipping. Not the claim in document 00.

**M6.** The same plus durable under crash and stable under memory pressure. The first version anyone should trust with real data.

**M10.** The claim in document 00, or a published and honest restatement of it.

**The failure modes, restated from document 02.7.** M1 says the format does not deliver the ratios: the resource axis is wrong and the spec is amended. M3 says encoded execution does not deliver the speed: the performance axis drops to 3 to 4x and the spec is amended. M5 says the three named problems do not yield: the target lands near 5x and the spec is amended. M7 runs long and the compatibility surface has no bottom: the target freezes at a named DuckDB version and that is stated publicly.

In every one of those cases the correct response is to amend the specification and keep the numbers honest, not to keep the claim and adjust the benchmark.

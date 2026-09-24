# The targets

What "win every benchmark" means for a compiled engine, one benchmark at a time. For each one: what the bar is, where the time goes, what a compiler can take out, what it cannot, and the budget the design has to fit.

Every number here is sourced in `research-notes/D-benchmarks.md` or `research-notes/C-joins-job.md` unless it is marked as ours. Numbers that are derived by arithmetic from published raw data are marked `[derived]`.

## 2.1 The rule for what counts

A claim is **end to end**: parse, bind, optimize, plan, generate code, compile, execute, and return the result. Compile time is never subtracted. Umbra's published JOB numbers subtract it, and that is exactly the number we are designing against.

A claim is **per query as well as total**. The per-query floor from `../02-the-goal.md` holds here unchanged: no query slower than DuckDB, on any suite, at any thread count we report. A compiler has a new way to break that floor, which is a short query that spends longer compiling than DuckDB spends running it. Section 2.8 turns that into a budget.

A claim names **the rival's version and the machine**. DuckDB is a moving target. It halved its JOB time between 0.10.1 and 1.3.2, and 1.5.x pushes join Bloom filters into probe-side scans. The baseline is the current DuckDB release measured by `rudb-bench` on the same machine in the same session, and the published numbers below are for orientation only.

A claim is **gated on instructions retired as well as wall time**, on the query-level harness in `rudb-bench` that already exists for TPC-H. Wall time on a laptop is noisy. Instructions are not, and "10x faster" at the same instructions per cycle means "one tenth the instructions".

## 2.2 JOB, the current focus

**The workload.** 113 queries from 33 templates over the IMDB dataset, about 3.6 GB of CSV. 3 to 16 joins per query, 8 on average. All of them are acyclic select-join blocks with outputs wrapped in `MIN(...)`. Selective filters sit on dimension tables, and there are LIKE and OR predicates over large text columns (`movie_info.info`, `movie_companies.note`, `title.title`, `name.name`). JOB was designed to break cardinality estimators, and it does.

**The published bars.**

| System | Threads | Execution | Compile | Source |
|---|---|---|---|---|
| DuckDB v0.9 | default | 18.210 s | n/a | Umbra raw data |
| DuckDB 0.10.1 | 1 | 123.9 s | n/a | AQP paper, read off figure |
| DuckDB 1.3.2 | 1 | 55.3 s | n/a | AQP paper, read off figure, Xeon E-2236 |
| Umbra | 32 | 0.928 s | 7.592 s | Umbra raw data |
| Umbra | 1 | 7.756 s | not stated | Umbra raw data |
| Umbra, lookup and filter variant | 32 | 0.870 s | about 88 s | Umbra raw data |

**Where the time goes, in DuckDB.** Oversized intermediates when an estimate is low. Hash join probes into tables that miss the last-level cache. LIKE over long strings. And, for the queries where all of that is small, per-query fixed overhead. DuckDB's average is about 0.5 s per query single-threaded, so fixed overhead is not its problem.

**Where the time goes, in Umbra.** Compilation. 7.592 s against 0.928 s of execution means compile is 89% of the end-to-end time `[derived]`. End to end, Umbra is about 8.5 s against DuckDB v0.9's 18.2 s, 2.1x, not the 20x its execution number suggests `[derived]`.

**The target.**

- Single-threaded, all 113 queries end to end: **one tenth of current DuckDB**. Against the 1.3.2 figure that is 5.5 s, 49 ms per query on average. Umbra's single-threaded execution alone is 7.756 s, so this requires beating Umbra's single-threaded execution by about 1.4x as well as not paying its compile cost.
- All threads, end to end: **one tenth of current DuckDB and faster than Umbra's execution-only number**, which means under 0.93 s end to end on comparable hardware.
- Per query: no query slower than DuckDB. The geometric mean speedup is reported next to the total, because the JOB total is dominated by a handful of templates.

**The compile budget that follows.** At 32 threads Umbra runs a JOB query in about 8.2 ms on average `[derived]`. To beat that end to end, compile has to be a small fraction of it. The budget, per query, measured on one core while the other cores are idle:

| Phase | Median | Max |
|---|---|---|
| parse, bind, rewrite (shared frontend) | 0.3 ms | 2 ms |
| physical planning, pipeline split | 0.1 ms | 0.5 ms |
| QIR generation | 0.1 ms | 0.5 ms |
| backend, `direct` | 0.5 ms | 2 ms |
| **total before first morsel** | **1.0 ms** | **5 ms** |

The backend line is the one that forces the design. A JOB query produces one pipeline per hash build plus the probe pipeline plus the aggregation, so 5 to 18 functions. DirectEmit compiled 6,678 TPC-DS functions in 64 ms, about 10 µs per function, so 18 functions is under 0.2 ms. Cranelift at 1.07 s for the same 6,678 functions is about 160 µs per function, so 18 functions is about 2.9 ms, over the median budget on its own. LLVM at -O2 is ten times that again. This is why document 08 makes the single-pass emitter the default, and the paragraph above is the whole argument.

The frontend line is not ours to design in this folder, but it is inside the claim. It is measured by C0 in document 18, and if the shared frontend misses 0.3 ms at the median, that is a finding for the frontend, not something to hide.

**What the compiler takes out, and what it does not.** Semi-join reduction, done as a plan stage, is worth about 1.5x on JOB by every published measurement (RPT 1.46x, RPT+ 1.47x, Parachute 1.54x, Yannakakis+ 1.42x mean). DuckDB 1.5.x is already absorbing part of that. The rest of the 10x has to come from the engine:

- **Staged, prefetching hash probes.** Group prefetching measured 2.7 to 3.7x over a naive probe when the table misses the cache.
- **The unchained hash table** with in-pointer Bloom tags. About 2x over open addressing on join-heavy probes.
- **Compiled LIKE.** Split on `%`, SIMD or Two-Way per segment, a German-string prefix fast path. Measured 13.3x over DuckDB on filter-bound work.
- **Late materialization that uses the `MIN` outputs.** Carry keys and row ids through the joins and fetch strings once at the end. `MIN` ignores duplicates, so n:m fan-out can be aggregated early or never expanded.
- **Filters compiled into scans at one to three instructions per tuple.** Both the transferred filters and the exact bitmaps from `../graph/`.

Document 10 is the design and document 17 is how each item's share is measured separately.

## 2.3 CEB and the robustness tail

CEB is thousands of queries (13,644 in the full set) over the same IMDB data, generated from 15 to 16 templates. It measures the same things as JOB but has no single query that can be fixed by hand. The published orientation point is CEB at scale factor 5, multi-threaded: Bespoke 0.6 s, Umbra 1.1 s, DuckDB 14.7 s `[snippet]`. On a CEB-style set from Umbra's raw data, DuckDB took 6,721 s against Umbra's 175 s plus 142 s of compile time.

For the compiler the lesson is the second pair of numbers. On a workload of thousands of distinct plans, compile time is 45% of Umbra's end-to-end time, and a code cache does not help because every plan is new. **CEB is the reason the compile budget in 2.2 is a per-query budget and not an amortized one.**

The target is the JOB target applied to CEB, plus one rule: the report includes the worst ratio against DuckDB across all queries, and a claim with a query below 1.0x is not a claim. Every robust join technique in the literature has a regression tail (RPT+ regresses 2.1% of SQLStorm queries, plain RPT at least 28%), and CEB is where ours will be found.

## 2.4 ClickBench

**The bar.** Summing the best hot run of each of 43 queries on `c6a.4xlarge`: Umbra 7.41 s, ClickHouse 18.00 s, Hyper 19.96 s, DuckDB 26.25 s, CedarDB 28.99 s. Ten times DuckDB is 2.63 s, 2.8x past Umbra. The board's default metric has been "combined" since September 2025, and the ranking is a geometric-mean score, so 10 ms queries count as much as 3 s queries.

**Where DuckDB's time goes.** Q29, a `REGEXP_REPLACE` over `Referer`, is 25% of the total on its own. Q29 plus Q35, Q34, Q33 and Q19 (string and wide-key `GROUP BY`) is 55%. The top 15 queries are 83%. On the LIKE queries Q22 and Q23 Umbra is 14 to 18x faster than DuckDB.

**What the compiler takes out.**

- Fused scan, filter and aggregate loops for the numeric queries.
- Hash table insert and probe specialized to packed key widths.
- Predicates evaluated on dictionary codes without per-vector dispatch.
- Pattern constants folded into specialized matchers. `'%google%'` becomes a SIMD substring search. The domain-extraction `REGEXP_REPLACE` in Q29 becomes a hand-shaped URL scanner, recognized from the pattern at plan time.

**What it does not take out.**

- Evaluating a regex once per distinct value instead of once per row. That is an encoding decision.
- Skipping data with zone maps, n-gram signatures or sort order.
- `COUNT(DISTINCT)` memory.
- Load time, on-disk size and cold I/O.

The first engine's storage work is the lever there, and the compiler consumes its results as facts.

**The compile budget.** Many ClickBench queries run in 10 to 30 ms on DuckDB, and under the geometric-mean score a 1 ms compile on a 3 ms query is a 33% loss on that query. The target is **0.3 ms median total compile per ClickBench query**. That is achievable because a ClickBench query is one to three pipelines. Queries that the policy in document 09 estimates at under 1 ms of execution start on the interpreter and never compile.

**The honest statement.** 10x DuckDB on ClickBench total requires Q29 at about one tenth of its current cost, and that is not a compiler result. The compiled engine's claim on ClickBench is: best per-query time on every query that is not dominated by string search or distinct-count memory, and no query worse than the first engine.

## 2.5 TPC-H

**The bar.** Single-threaded at SF20, from the Bespoke OLAP paper: DuckDB 1.4.1 49.2 s, Umbra about 31.9 s `[derived]`, the synthesized engines 4.4 s. Multi-threaded at SF50: DuckDB 8.9 s, Umbra 7.2 s, Bespoke 1.2 s. Bespoke's 11x over DuckDB is the ceiling of what specialized code plus specialized storage can do. In the paper's 2x2 ablation, optimized code over a flat layout got 5.18x on TPC-H and 8.09x on CEB, against 1.26x and 0.57x for basic code over the same layout. Code that knows the data is most of the multiplier (document 01 section 1.10).

**Where rudb is.** Parity with DuckDB on SF1 wall time at 1.51x its instructions. The worst queries by instruction ratio are q01 at 2.34x and q12 at 1.96x (`rudb-bench/reports/2026-09-24`). The target at SF1 is 1.79G instructions for the suite, a 15x cut.

**What the compiler takes out.** This is the benchmark where the compiler does the most.

- q01 is the canonical compute-bound pipeline. Kersten et al. measured the compiled engine 74% faster on it, and Impala measured 5.7x from codegen alone.
- Register-resident aggregate state, and dense array aggregation where the key range is known.
- Decimal arithmetic inlined as checked 64-bit or 128-bit integer operations.
- Hash probes at about ten instructions per lookup, which is CedarDB's number.
- Groupjoin for the queries that join and then group on the same key.

**What it does not take out.** The join order, predicate transfer (1.44x on TPC-H), the sort order of `lineitem` on disk, and memory bandwidth on the scan-bound queries at SF100 once the code is tight.

**The compile budget.** Umbra's published SF1 data has 1.10 s of optimized compile time against 0.107 s of execution for the 22 queries. That is the failure mode at small scale factors. The budget is the JOB budget, 1 ms median, and the adaptive policy moves long pipelines to `clif` in the background at SF100.

**The target.** Instructions at one tenth of DuckDB at SF1. Wall time at one tenth of DuckDB at SF100 single-threaded, and at least parity with Umbra's execution time end to end.

## 2.6 TPC-DS

**The bar.** There are almost no public totals. DuckDB on a MacBook Neo ran SF300 in 79 minutes, 51 of them on Q67. DuckDB plus RPT measured 1.56x over DuckDB.

**What the compiler takes out.** Per-operator efficiency of window functions and grouping sets, star-join probe chains fused into one loop across several dimension tables, and Bloom probe loops. Bloom filter work is 46% of the time under RPT on TPC-DS, so a cheap compiled probe matters more here than on JOB, where it is 12%.

**What it does not.** Decorrelation, CTE materialization choices, grouping-set planning, spilling algorithms, and coverage of all 99 queries. On TPC-DS a missing feature is a loss, and the router in document 03 turns a missing translator into a first-engine execution rather than an error. That keeps the floor. It does not win.

**The target.** All 99 queries run, at least 90 of them on the compiled engine by C10, none slower than DuckDB, and 5x on the total at SF100. Not 10x, for the reason `../02-the-goal.md` gives: the TPC-DS total is set by a few planner-bound outliers.

## 2.7 TPC-C

**The bar.** DuckDB has no published TPC-C and says in its documentation that many small concurrent transactions are not a goal. An indexed-column `UPDATE` there is a delete plus an insert. Beating DuckDB is easy and not interesting. The meaningful single-thread references are:

- HyPer: 126,576 tps on 12 warehouses without concurrency control.
- Umbra: 27,000 transactions per second on 100 warehouses with snapshot-isolation MVCC.
- LeanStore: about 41,000.
- PostgreSQL: about 2,600.

Hekaton's compiled procedures ran 15.7x faster than interpreted SQL Server.

**Where the time goes.** Instructions per transaction. A vectorized engine spends 10^5 to 10^6 instructions per statement on parsing, planning and executor setup. Compiled procedures bring a whole transaction to about 10^4. After that come index probes, MVCC bookkeeping, latching and the log.

**What the compiler takes out.** Everything that is per-statement overhead, but only if the compiled code is reused. A New-Order transaction lasts microseconds and a compile takes about a millisecond, so compiling on every execution is a 100x loss. Prepared statements compile once to a parameterized function, the hot path is a cache lookup keyed by statement identity, and point statements compile to one function with the index probe inlined and no morsel machinery. Document 14.

**What it does not take out.** Umbra's ablation measured 5.1x from in-place updates alone, and that is a storage decision, as are the index structure, the log and group commit. `../engine-v4/` owns those.

**The target.** On one thread at 100 warehouses, with the storage work in `../engine-v4/` in place: 10x DuckDB, which is not hard, and within 2x of Umbra's 27,000 TX/s. On the compiler alone, the measurable target is **under 20,000 instructions of statement overhead per transaction** from cache lookup to first index probe. That is instrumentable before the storage work exists.

## 2.8 The one budget that applies everywhere

**The time to first morsel on a cache miss is under 1 ms at the median and 5 ms at the maximum for any query in any of the suites above. The total code generation and backend time summed over every pipeline of the query is held to the same 1 ms and 5 ms.** The first half bounds latency. The second half stops the first half being met by deferring compile work into the query's later pipelines, where it would still be on the critical path. Pipelines whose input is provably at most one morsel, from storage metadata and never from an estimate, run on the interpreter (document 09, Rule I1). Everything else is compiled on `direct` when it becomes runnable, within that budget, and moves to `clif` in the background only if the extrapolation in document 09 says the remaining execution time justifies it.

This one number replaces every per-benchmark compile concern above. If it holds, JOB, CEB, small-scale TPC-H and the short ClickBench queries are all safe from compile latency. If it does not hold, no amount of execution speed makes up for it on JOB. It is gate G1 in document 18 and it is measured before any execution optimization is attempted.

## 2.9 How the 10x is expected to decompose

This is a prediction, stated so that document 17 can check it query by query and so that a component that underdelivers is noticed rather than absorbed.

| | JOB | TPC-H | ClickBench | TPC-C |
|---|---|---|---|---|
| plan: reduction, order, unnesting | 1.5x | 1.4x | 1.0x | 1.0x |
| layout and encoded facts consumed by code | 1.5x | 2x | 3x | n/a |
| fused compiled pipelines, register state | 1.5x | 2.5x | 1.5x | 3x |
| staged probes, prefetch, hash table | 2x | 1.5x | 1.3x | n/a |
| compiled strings and patterns | 1.5x | 1.0x | 2x | n/a |
| statement overhead and code reuse | 1.0x | 1.0x | 1.0x | 5x |
| **product** | **10x** | **10.5x** | **11.7x** | **15x** |

Three cautions about this table.

1. **The factors are not independent.** Staged probes matter less once reduction has removed the tuples that would have missed. Compiled strings matter less once the dictionary has been evaluated. The product overstates what the sum of individually measured wins will be.
2. **The ClickBench column needs Q29 solved outside the compiler.** Without that, the column is about 4x.
3. **The TPC-C column assumes `../engine-v4/` is in place.** Without in-place updates and an OLTP index, the compiler's factor applies to a storage engine that caps throughput first.

The column that matters most is JOB, and every factor in it has a published measurement behind it in document 01. That is not the same as having measured their product, which is document 17's job.

## What we should take from this document

A benchmark win counts only end to end, compile included, per query as well as in total, against a named DuckDB version, and backed by instructions retired, not wall time alone.

JOB decides the design. At Umbra's compile cost, compilation is 89% of JOB end to end, so the compiled engine's first number is G1: at most 1 ms median and 5 ms maximum to first morsel, and in total compile per query. Everything else is spent inside that budget.

The 10x on each benchmark is a product of five or six mechanisms, each with a published measurement, and the product is a prediction, not a measurement. Document 17's ablation switches check it factor by factor.

Two columns depend on work outside the compiler: ClickBench's Q29 and TPC-C's storage engine. They are named so that neither is claimed for the compiler.


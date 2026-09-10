# The plan

This is the executable form of the ten layers. Thirteen sub-milestones, each with what has to be true before it starts, what gets built, what test proves it correct, what benchmark proves it worth doing, and what has to be true before it is called done.

The sub-milestones are 2a through 2m because they all sit under M2 in the milestone plan, which is titled "A working database" and which was previously ordered by how many corpus records each feature would unlock. That ordering is replaced by this one and document 00 says why.

## 14.1 The table

| | name | layer | document | depends on |
|---|---|---|---|---|
| 2a | The baseline | none | [02](02-baseline.md) | nothing |
| 2b | The data plane | 1 | [03](03-data-plane.md) | 2a |
| 2c | Expressions | 2 | [04](04-expressions.md) | 2b |
| 2d | Parquet and async I/O | 3 | [05](05-scan.md) | 2c |
| 2e | The fast scan | 3 | [05](05-scan.md) | 2d |
| 2f | The hash table | 4 | [06](06-hash.md) | 2b, 2c |
| 2g | Aggregation | 5 | [07](07-aggregate.md) | 2e, 2f |
| 2h | Join | 6 | [08](08-join.md) | 2f, 2g |
| 2i | Sort, top-N and window | 7 | [09](09-sort-and-window.md) | 2c, 2f |
| 2j | The scheduler, memory and spilling | 8 | [10](10-scheduler.md) | 2g, 2h, 2i |
| 2k | The optimizer | 9 | [11](11-optimizer.md) | 2h, 2j |
| 2l | Adaptivity | 10 | [12](12-adaptivity.md) | 2k |
| 2m | The write path | none | section 14.4 | 2e |

The dependency column is what actually constrains the order, and it is looser than the alphabetical sequence. 2f depends on the data plane and the expressions and not on the scan, so it can be built in parallel with 2d and 2e by a second person. 2i depends on 2c and 2f and not on the join, so it can run alongside 2h. Nothing after 2j is parallelizable, because the scheduler touches every operator and merging a large operator change across it is worse than waiting.

The realistic sequential path for one person is 2a, 2b, 2c, 2d, 2e, 2f, 2g, 2h, 2i, 2j, 2k, 2l, with 2m inserted whenever the write path becomes the thing blocking a real user rather than a benchmark.

## 14.2 The gates, in one place

Each row is the number that has to move, on which machine, against what.

| | benchmark gate | target |
|---|---|---|
| 2a | all suites, all engines | the table exists and is honest, including rudb abstaining on the large suites |
| 2b | `kernels`, plus ClickBench Q1 to Q5 and TPC-H Q1 and Q6 | CPU seconds down 5x against 2a |
| 2c | `expressions`, plus TPC-H Q6, ClickBench Q20 to Q27 and Q29 | TPC-H Q6 down 3x, ClickBench string queries down 2x, both against 2b |
| 2d | ClickBench, all 43, on the full `hits.parquet` | every answer matches DuckDB, times within 3x of DuckDB on the scan queries |
| 2e | ClickBench scan and filter queries, native format | fewer bytes read and fewer CPU seconds than DuckDB |
| 2f | hash microbenchmarks, plus the ClickBench `GROUP BY` queries | CPU seconds down 10x against 2e, probe throughput per core at or above DuckDB |
| 2g | TPC-H Q1, ClickBench Q4 Q5 Q7 and Q28 to Q33 | beat DuckDB on TPC-H Q1 single threaded on `server3` |
| 2h | TPC-H SF100 all 22, on `server1` | within 2x of DuckDB single threaded, probe throughput per core above DuckDB |
| 2i | sort and top-N microbenchmarks, ClickBench `ORDER BY ... LIMIT` queries | beat DuckDB on a 100M row single-key sort single threaded |
| 2j | scaling curves at 1, 2, 4, 8 threads, and SF100 under a 1 GB limit on `server1` | near-linear scaling, CPU seconds flat within 20 percent, every query completes under the limit |
| 2k | JOB, CEB, TPC-DS all 99, TPC-H SF100 | beat DuckDB on total CPU seconds across TPC-H SF100 |
| 2l | the worst-case four-number table per decision | adaptive within the exploration bound of the better fixed choice on both adversaries |
| 2m | load time on `hits` and SF100 | within 2x of DuckDB's load time |

Two of those targets are stated as within a factor rather than as a win, 2d and 2h, and both have a written reason: at 2d the aggregate and the join are still slow and beating DuckDB would mean the reader is skipping work, and at 2h there is no optimizer so several TPC-H queries are running plans a cost model would not have chosen. Every other target is a win.

## 14.3 What each one is

**2a. The baseline.** Turn the `Rudb` stub in `rudb-bench` into a subprocess engine driven through `rudb-cli`, add ClickHouse in both configurations, DataFusion and Polars, add CPU seconds measurement, wire `cargo xtask bench`, add the CI regression gate on `smoke`, run everything on `server3` and `server1`, publish the table with rudb abstaining on `clickbench` and `tpch` at SF100, and write the first ledger row. Documents [02](02-baseline.md) and [13](13-measurement.md).

**2b. The data plane.** Rewrite the five kernel files against a dispatch-once interface with no `Value` on any loop path, collapse stacked dictionaries, add `Chunk::compact` and measure the compaction surface, use the string prefix, replace the block vector with an arena, add the refcounted pin seam and the non-exhaustive `Form`, sweep the vector size, and add the lint that keeps `Value` off the hot path. Document [03](03-data-plane.md).

**2c. Expressions.** Prepare expressions once per pipeline with reusable scratch and resolved column positions, thread selections through conjunctions with adaptive ordering, rewrite `CASE`, give functions a prepare step so patterns and casts compile once, and produce the number that decides whether tier 1 fusion is worth scheduling. Document [04](04-expressions.md).

**2d. Parquet and async I/O.** Add the submission interface to `File` and the I/O thread pool, write the Thrift decoder, the page decoders, the level decoding and Snappy, wire projection pushdown, and run all forty-three ClickBench queries end to end against the real file for the first time. Document [05](05-scan.md) sections 5.3 to 5.6.

**2e. The fast scan.** Add the native format reader, row group and page pruning and Bloom filters, late materialization with the encoding-aware cost term, and predicates evaluated on dictionary, bit-packed, FSST and run-length data with property tests against decoded references. Document [05](05-scan.md) sections 5.6 to 5.8.

**2f. The hash table.** One key encoding, one hash function, four specialized key cases, an unchained table for the join shape and a salted open-addressed one for the aggregate shape, concurrent insert, prefetched batch probing, byte accounting and hash-bit partitioning seams. Document [06](06-hash.md).

**2g. Aggregation.** Aggregates become a state size and five functions with vectorized scatter update, three grouping shapes chosen at plan time, `COUNT(DISTINCT)` as a second aggregation, `count(*)` and unfiltered extremes from statistics, the sink contract, and the aggregate catalogue grown past six. Document [07](07-aggregate.md).

**2h. Join.** Hash join on the layer four table with a row layout build side and a resumable probe, all eight kinds checked against the nested loop oracle, semi and anti without payload, a Bloom filter and derived range predicate pushed into the probe scan, and the nested loop kept as the non-equi fallback and the reference. Document [08](08-join.md).

**2i. Sort, top-N and window.** Radix sort over normalized keys with the payload gathered once, `default_null_order` honoured, parallel sink with a range-partitioned merge, top-N as a bounded heap, and the `Window` node built through parser, binder, planner and executor with all three function classes and all three frame implementations. Document [09](09-sort-and-window.md). Merge join, IEJoin, `AsOf` and ordered aggregates follow immediately.

**2j. The scheduler, memory and spilling.** Push-based pipelines with morsel-driven tasks over a shared pool, pipeline dependency scheduling, four blocking reasons with no cycles, a buffer manager with pinning and eviction, real memory accounting so `memory_limit` and `max_execution_time` become honoured, spilling in the aggregate and the sort and the join, `WITH RECURSIVE`, cancellation, and `EXPLAIN ANALYZE` with blocked time by reason. Document [10](10-scheduler.md).

**2k. The optimizer.** Toggleable rewrite passes for filter and projection pushdown, folding and simplification and common subexpression elimination, arbitrary subquery unnesting with the five special shapes, DP join ordering with a planning time budget and a greedy fallback, predicate transfer with its build-or-not decision, and a cost model fitted to the microbenchmarks driven by the existing per-column sketches with the correlation term. Document [11](11-optimizer.md).

**2l. Adaptivity.** One framework, the nine decisions as its instances, identical-answer alternatives with hysteresis and a decaying bounded exploration schedule, mid-query re-optimization restricted to exact cardinalities at materialization points, all reported and all disableable. Document [12](12-adaptivity.md).

**2m. The write path.** Section 14.4.

## 14.4 The write path, which is not a layer

Encode runs at 5 MB/s of values a core after front coding, which is 5.6 CPU hours for `hits` and 105 minutes of wall clock on four cores. DuckDB loads the same file in minutes. The changelog already records that a write path twenty times slower than the read path is not shippable.

It is not in the ten layers because it is not query execution. It is the storage format's write side, it belongs to M1 and to documents 05 and 06 of the parent spec, and it is listed here because a plan that never schedules it is a plan that ships a database nobody can load data into.

The known causes, in the order they are worth investigating: the chooser evaluates too many candidate encoding combinations per column and its search is not pruned by the sketches it already computed; encoding is per column and single threaded where it is embarrassingly parallel across columns and across blocks; and the front coding pass added at the end of M1 is the slowest single stage and was never profiled because it was measured for size rather than for time.

The gate is load time within a factor of two of DuckDB on `hits` and on TPC-H SF100, with the on-disk size not regressing past 0.50 of DuckDB's, because a write path made fast by encoding worse is not a fix. Both numbers are already in the ledger from 2a.

It is scheduled whenever the write path is what blocks a real user rather than what embarrasses a benchmark, and the honest expectation is that this happens shortly after 2e, because 2e is when rudb's native format becomes the thing queries read and therefore the thing data has to be written into.

## 14.5 What closes M1 first

Two boxes remain on issue 2 and neither is engine work.

The standalone encoder needs to ingest TPC-H SF100, which is generating on `server1`, plus a handful of real Parquet datasets beyond `hits`. That is a run rather than a build.

The M1 report has to be written and published in `rudb-bench`, which is the M1 exit criterion. It says the format result: 9.65 GB against Parquet's 13.76 and DuckDB's 20.46, which is 0.70 and 0.47, the six-level encoding shape the chooser found for URL, the encode and decode rates, and the peak resident numbers.

It also has to say the thing the exit criterion asks for, which is that at 9.65 GB the parent spec's ten times resource claim is wrong and is no longer explainable by a single column. Documents 00, 02 and 03 of the parent spec are amended accordingly, and the exit criterion says amending the specification is the success case here rather than the failure case.

That report and those amendments happen before 2a, because 2a's first table needs the size numbers in it and because a specification that still claims ten times on a measured 2.1 times is a specification nobody can plan against.

## 14.6 The parked settings branch

`set-reset-pragma` at `1a5f24e` has `rudb-common/src/settings.rs`, which classifies every DuckDB configuration name by the rule that a setting is refused when ignoring it would change which rows a query gives back. It is written, tested and clean, and it is not wired to the parser, the binder or `Database`, so it is a table and not a feature.

Two settings in it are classified as remembered with an argument that points at future work, and this plan is where that work lands. `default_null_order` becomes honoured at 2i, because that is where the sort learns to order nulls. `memory_limit` and `max_execution_time` become honoured at 2j, because that is where a memory manager and a cancellable scheduler exist.

So the branch is not merged as a feature now. It is merged as the table, with the classification and the tests, at 2a or 2b where it costs nothing, and the two settings it defers are ticked off by 2i and 2j. That is better than leaving it parked, because the classification is the part that took the work and the part that is easy to lose.

## 14.7 Issues, labels and releases

Each sub-milestone is one GitHub issue on the M2 milestone, titled with its letter, holding the checklist from its document's exit criterion, with the benchmark gate from section 14.2 stated in the body so that closing it is a matter of pointing at a number.

Labels: `kind/milestone` and `kind/perf` on every one, `priority/p0` on 2a through 2e because nothing else can be measured or run without them and `priority/p1` on the rest, plus the area labels for the crates each one touches. 2b is `area/execution` and `area/types`, 2d and 2e are `area/io`, `area/parquet`, `area/storage` and `area/encoding`, 2f through 2i are `area/execution`, 2j is `area/execution` and `area/io`, 2k is `area/optimizer`, 2l is `area/optimizer` and `area/perf`, and 2m is `area/encoding` and `area/storage`. 2a and every benchmark item also carry `area/testing`.

The checklist on issue 3, which is M2, is replaced by the thirteen sub-milestone references, and a comment on it explains the reordering with the reasoning from document 00 rather than silently rewriting the list.

Releases follow the existing convention. Each sub-milestone closes with a patch release, so 2a is v0.1.x and the letters march through the patch numbers, with the release notes written from the ledger row that sub-milestone produced. M2 completing is v0.2.0. Every release publishes to crates.io.

The one addition to the convention: a release whose ledger row shows no movement on any suite does not get release notes claiming a performance improvement. If a sub-milestone lands and the number did not move, the release note says so and the next thing to do is find out why, because document 00's second consequence is that a layer which makes a microbenchmark faster and moves no query was not on the critical path, and finding that out cheaply is the point.

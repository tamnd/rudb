# D. Benchmarks: what "win every benchmark" means (ClickBench, TPC-H, TPC-DS, JOB/CEB, TPC-C)

Research date: 2026-09-24. Target: rudb (Rust, DuckDB-compatible, goal ~10x DuckDB) with a query-compiling execution engine.

Conventions:
- `[snippet]` means the number was seen only in a search-result snippet and not verified against the primary page.
- `[derived]` means I computed it from primary numbers (the method is stated).
- `[computed]` means summed or scored by me from raw result JSON or CSV. No official total exists.
- All times are seconds unless marked otherwise.

## 0. TL;DR

- **ClickBench, c6a.4xlarge, hot.** Umbra leads with a 7.41s hot sum and a relative score of about 1.35. DuckDB is at 26.25s (score about 5.2) and ClickHouse at 18.00s.
  - 10x DuckDB's hot sum is 2.63s. That is 2.8x faster than Umbra today.
  - Only about 3.4x DuckDB is needed to take #1 on the hot sum. On the relative score, the per-query minimum matters, not the sum.
- **ClickBench is dominated by a handful of string-heavy queries.** In DuckDB's time, Q29 (REGEXP_REPLACE on Referer) alone is 25%. Q29 plus Q35/Q34/Q33/Q19 are 55%. The top 15 queries are 83%.
- **TPC-H.** Umbra and CedarDB are the in-memory leaders.
  - DuckDB 1.4.1 single-threaded at SF20 is 49.2s. Umbra is about 31.9s `[derived]`. Bespoke-synthesized code is 4.4s.
  - Multi-threaded at SF50: DuckDB 8.9s, Umbra 7.2s, Bespoke 1.2s.
  - A 10x-DuckDB TPC-H is roughly "Bespoke OLAP level". It is not achievable by a generic compiler alone, since Umbra is only 1.2-1.6x DuckDB there.
- **JOB/CEB.** Umbra's execution time is about 20x lower than DuckDB's on JOB (0.93s vs 18.2s, different versions and threads).
  - Umbra's compile time is 7.6s for 113 queries, 8x its execution time.
  - On JOB, compile latency and join order and predicate transfer matter more than tight loops.
- **TPC-C.** DuckDB has no published TPC-C and says small concurrent transactions are not a goal.
  - Compiled OLTP references at one thread: Umbra 27k TX/s (100 warehouses) and HyPer 126k tps (12 warehouses, no concurrency control, stored procedures).
  - Hekaton: native procedures do 15.7x the throughput of interpreted SQL Server.
- **rudb today:**
  - TPC-H SF1, single thread: about 0.93-0.98x DuckDB's time (roughly parity) while retiring 1.51x the instructions.
  - ClickBench full file: hot 27.07s vs DuckDB 14.03s (on gamingpc-wsl), with 42/43 queries answered.
  - Q21/Q22 slice benchmarks: up to 3.13x ahead.

## 1. ClickBench

Sources:
- Repo: https://github.com/ClickHouse/ClickBench. Sparse clone at /tmp/ClickBench, commit dated 2026-09-24.
- Changelog: https://raw.githubusercontent.com/ClickHouse/ClickBench/main/CHANGELOG.md
- Board: https://benchmark.clickhouse.com/

### 1.1 Scoring rules (as of 2026-09)

- 43 queries over a single table, `hits`, with 99,997,497 rows. Each query runs 3 times.
  - Cold is try 1.
  - Hot is the minimum of tries 2 and 3. The site uses min(tries) for hot.
- Relative score for a system is the geometric mean over queries of `(t + 0.01) / (best_t + 0.01)`, where `best_t` is the best time for that query among the selected systems. Missing or failed queries are penalized (I used 300s in my approximation). Lower is better, and 1.0 means best on every query.
- **Default metric changed to "combined" on 2025-09-10.** Combined weights (changelog 2025-07-04): 10% load time, 10% data size, 60% hot, 20% cold.
- 2025-07-11: smaller machines added (c6a.large, c6a.xlarge, t3a.small and similar). Umbra, CedarDB and Hyper drop out of machines with 16GB of RAM or less.
- 2025-10-26: in-memory systems (duckdb-memory, the Hyper variants and others) excluded from the cold and combined rankings.
- 2025-12-10: Sirius (GPU, DuckDB extension) added.
- 2026-05-11: unified runner.
  - The OS page cache is dropped and the server restarted before the cold run.
  - Embedded engines are wrapped in an HTTP server so that they are also "restarted".
  - A concurrent QPS metric was added.
- 2026-08-28: "no-cold" tag, for systems that cannot do a true cold run.
- 2026-09-18: Storage selector (local / object storage / in-memory) on the board.

Implication: a DuckDB-compatible embedded engine is scored like a server. The process restarts between cold runs, and hot runs keep the process alive.

### 1.2 c6a.4xlarge (16 vCPU, 32GB, EBS gp2 500GB)

[computed] from /tmp/ClickBench/*/results/*c6a.4xlarge*.json with /tmp/cb.py. The score is my approximation: hot relative score over all systems present, with a missing query counted as 300s.

| system | result date | ~score (hot) | hot sum | cold sum | notes |
|---|---|---|---|---|---|
| Umbra | 2026-09-18 | 1.35 | 7.41 | 47.27 | 8.30GB data |
| CedarDB | 2026-08-15 | 1.86 | 28.99 | - | sum hurt by a few slow queries; per-query often best |
| Hyper | 2026-09-14 | 2.26 | 19.96 | - | |
| hyper-web | | 2.34 | 20.31 | | |
| intent-gizmosql | | 2.45 | 14.74 | | |
| gizmosql | | 2.96 | 21.03 | | DuckDB behind Arrow Flight SQL |
| clickhouse-web | | 3.14 | 17.44 | | |
| mariadb-duckdb | | 3.17 | 23.38 | | |
| duckdb-memory | | 3.18 | 22.08 | | |
| Firebolt | | 3.31 | 15.57 | | |
| ClickHouse | 2026-09-24 | 3.34 | 18.00 | 115.11 | |
| Ursa | | 3.65 | 26.15 | | |
| umbra-parquet | | 4.50 | 23.94 | | |
| **DuckDB** (likely v1.5.2) | 2026-05-11 | **5.20** | **26.25** | **115.19** | load 126s, 20.46GB |
| chdb | | 6.55 | 31.04 | | |
| Polars | | 6.78 | 45.35 | | |
| StarRocks | | 7.44 | 44.47 | | |
| DataFusion | | 8.62 | 45.57 | | |
| Doris | | 8.81 | 48.43 | | |
| Sail | | 10.20 | 54.94 | | |
| gendb | | 11.65 | 86.29 | | LLM-generated engine entry |
| Velox | | 25.21 | 81.51 | | |

Observations:
- The hot sum and the score rank differently. CedarDB is #2 by score and has a mid-pack sum, because the geometric mean rewards being best on many cheap queries.
- To be #1 by score you must be within a small factor of the best system on every query. One 10x-slow query costs about 10^(1/43), roughly 5.5% of score.
- 10x DuckDB hot sum = 2.63s. Umbra's 7.41s is 3.54x DuckDB.
- The DuckDB 1.4 LTS post (https://duckdb.org/2025/10/09/benchmark-results-14-lts) claimed #1 in-memory on ClickBench, and #1 open-source behind Umbra overall. That was under older rules.

### 1.3 c6a.metal (192 vCPU) and c7a.metal-48xl (192 vCPU), hot sums [computed]

| system | c6a.metal hot | c7a.metal-48xl hot |
|---|---|---|
| Umbra | 3.80 | 1.33 (score ~1.04) |
| Hyper | 3.73 | 2.78 |
| CedarDB | 4.52 | 2.53 |
| Firebolt | 3.27 | - |
| ClickHouse | 4.72 | 3.27 |
| Polars | - | 7.44 |
| Doris | 11.03 | - |
| DataFusion | 14.79 | 9.78 |
| DuckDB | 17.32 (score ~6.73) | 15.70 (score ~8.13) |

- DuckDB scales poorly from 16 to 192 vCPU: 26.25 to 17.32 (1.5x). Umbra goes 7.41 to 3.80 on c6a.metal (1.95x) and reaches 1.33 on c7a.
- On big machines, fixed per-query overhead and serial phases dominate. Examples are the final merge of high-cardinality aggregations, string dictionary work, and a single-threaded regex. The 10x-over-DuckDB target is easier on metal (Umbra is already 11.8x DuckDB on c7a) and hardest on c6a.4xlarge.
- GPU: Sirius has a hot sum of 1.45s on GH200 and 1.50s on p5.4xlarge [computed].

### 1.4 Which queries dominate DuckDB's time (c6a.4xlarge, hot)

[computed] from the DuckDB and Umbra JSON. The cumulative column is the share of DuckDB's 26.25s.

| Q | shape | DuckDB | Umbra | DuckDB/Umbra | cum. share |
|---|---|---|---|---|---|
| Q29 | `REGEXP_REPLACE(Referer, '^https?://(?:www\.)?([^/]+)/.*$', '\1')` group by, HAVING count>100000 | 6.478 | 1.371 | 4.7x | 25% |
| Q35 | GROUP BY 1, URL (high-card string) | 2.198 | 0.717 | 3.1x | |
| Q34 | GROUP BY URL | 2.054 | 0.719 | 2.9x | |
| Q33 | GROUP BY WatchID, ClientIP (~100M groups) | 2.035 | 0.991 | 2.1x | |
| Q19 | GROUP BY UserID, extract(minute), SearchPhrase | 1.650 | 0.729 | 2.3x | 55% |
| Q23 | `Title LIKE '%Google%'` + URL not like, group by SearchPhrase | 1.169 | 0.065 | 18x | |
| Q17 | GROUP BY UserID, SearchPhrase LIMIT | 0.892 | 0.359 | 2.5x | |
| Q14 | GROUP BY SearchEngineID, SearchPhrase | 0.781 | 0.305 | 2.6x | |
| Q22 | `URL LIKE '%google%'` + SearchPhrase group by | 0.769 | 0.055 | 14x | |
| Q21 | `URL LIKE '%google%'` count | 0.711 | 0.136 | 5.2x | |
| Q18 | GROUP BY UserID, SearchPhrase (no order) | 0.659 | 0.186 | 3.5x | |
| Q28 | CounterID avg(strlen(URL)) HAVING | 0.645 | 0.101 | 6.4x | |
| Q10 | MobilePhoneModel count distinct UserID | 0.612 | 0.209 | 2.9x | |
| Q32 | GROUP BY WatchID, ClientIP with filter | 0.611 | 0.124 | 4.9x | |
| Q15 | GROUP BY UserID top | 0.473 | 0.180 | 2.6x | 83% |

Reference totals: DuckDB 26.25, Umbra 7.41, ClickHouse 18.00, Hyper 19.96. On Q29, ClickHouse is 1.607 and Hyper 3.716.

What this says for a compiler:
- **Q29 is a regex-engine problem, not a codegen problem.**
  - Umbra wins it with a faster regex and domain extraction, plus probable dictionary-level evaluation.
  - Evaluating the regex once per distinct Referer (dictionary-encoded) instead of once per row is the big lever.
  - Special-casing this regex pattern into a hand-coded URL domain extraction is another route. ClickHouse has `domain()`.
- **The LIKE queries (Q21-Q23, 14-18x gap to Umbra) are about substring search over compressed strings.**
  - Tools: SIMD memmem, FSST-compressed matching, and skipping via n-gram or bloom signatures.
  - rudb's "native-gram-sieve" work targets exactly this (section 8).
- **High-cardinality GROUP BY on strings or wide keys (Q33-Q35, Q19, Q17, Q18, Q14)** depends on:
  - hash table design, partitioning or radix, and string key handling (dictionary codes vs. full strings);
  - a parallel final merge;
  - memory bandwidth.
  - Compilation helps with key packing and hash-probe loops. CedarDB's hash table blog cites about 10 instructions per lookup.
- **COUNT(DISTINCT) at high cardinality (Q5, Q6, Q10).** Memory is the constraint. rudb OOMed on Q6, COUNT(DISTINCT SearchPhrase), in issue #755.

### 1.5 ClickBench critiques relevant to "winning"

- **QuestDB, "Lies, damn lies, and database benchmarks"** (https://questdb.com/blog/lies-damn-lies-and-database-benchmarks/):
  - Keeping the DuckDB process alive (no restart) gives 1.24-1.69x.
  - Hyper gains 2.17x from the same.
  - The relative score shifts when the set of systems changes, because the baseline is the per-query best.
  - Cites ClickBench issue #936.
  - Methodology choices move the ranking as much as engine work does.
- **Structural criticisms (general, widely repeated):**
  - one denormalized table with no joins;
  - no concurrency in the main metric (a QPS metric was added 2026-05-11);
  - the sum is dominated by a few queries (see 1.4);
  - it rewards pre-sorted or pre-indexed tuned storage;
  - "hot" rewards result and data caching.
- **Rule churn** (restart-before-cold, in-memory exclusion, combined default) means a claimed ranking must name the date and metric.

## 2. TPC-H

### 2.1 Academic and vendor comparisons with numbers

**Bespoke OLAP** (arXiv 2603.02001 v2, https://arxiv.org/abs/2603.02001). Engines are LLM-synthesized and specialized per workload.

Setup: DuckDB 1.4.1 and Umbra 26.02, AMD EPYC 9654P with 768GB, in-memory.

Single-threaded:

| workload | Bespoke | DuckDB | Umbra | Bespoke/DuckDB | Bespoke/Umbra |
|---|---|---|---|---|---|
| TPC-H SF20 | 4.4s | 49.2s | ~31.9s [derived] | 11.17x | 7.24x |
| CEB SF2 | 0.4s | 19.5s | ~3.8s [derived] | 45.33x | 9.56x |

Umbra's times are derived as Bespoke time × speedup and are approximate.

Multi-threaded (Fig. 1):

| workload | DuckDB | Umbra | Bespoke | Bespoke/DuckDB | Bespoke/Umbra |
|---|---|---|---|---|---|
| TPC-H SF50 | 8.9s | 7.2s | 1.2s | 7.65x | 6.12x |
| CEB SF5 | 14.7s | 1.1s | 0.6s | 23.97x | 1.87x |

Ablation on storage:
- Flat storage alone gives 1.26x on TPC-H.
- Bespoke storage takes it to 12.35x (TPC-H) and 51.40x (CEB).
- **Most of the win is storage layout and data-aware physical design, not codegen per se.**

Strategies used by the synthesized engines:

| strategy | share of queries |
|---|---|
| fused aggregation | 97% |
| bitmap semi-join | 74% |
| dense-key (array) aggregation | 55% |
| index nested-loop join | 42% |

They also use lineitem sorted by shipdate with zone maps, and a precomputed discounted price (`l_extendedprice*(1-l_discount)`).

Caveat: workload-specific. Correctness is only for the known query templates. This is not a general DBMS.

**Diamond hardware / VLDB 2024 raw data** (umbra-db/diamond-vldb2024 `benchmark_data`, cloned at /tmp/diamond). [computed] sum of per-query median exec times:

| system | config | TPC-H SF1 exec | TPC-H SF10 exec | compile |
|---|---|---|---|---|
| DuckDB v0.9 (2023) | memory_limit 50GB, threads unknown | - | 6.945s | n/a |
| Umbra base (v06.base) | 32 threads, compilationmode=o | 0.107s | 0.957s | 1.103s (SF1) / 1.123s (SF10) |

- At SF1, **Umbra compile (1.10s) is 10x its execution (0.107s)** for the 22 queries in optimized mode.
- In practice, Umbra's adaptive mode (Flying Start / interpreter-style bytecode first, then LLVM) avoids paying this. The lesson: an LLVM-only compiler loses small-SF TPC-H outright.

**CedarDB** (https://cedardb.com/blog/simple_efficient_hash_tables/):
- About 6x faster than Hyper and DuckDB on TPC-H SF100 on a Ryzen 5950X.
- Hash table lookup takes about 10 instructions, with 4-bit Bloom-style tags in the pointer.
- Tinybird reports CedarDB compile latency of 50-100ms per query [snippet].

**Robust Predicate Transfer** ("Debunking the Myth of Join Ordering", SIGMOD'25, arXiv 2502.15181), single thread, speedups over DuckDB:

| technique | TPC-H | JOB | TPC-DS | DSB |
|---|---|---|---|---|
| RPT | 1.44x | 1.46x | 1.56x | 1.54x |
| Bloom join only | 1.15x | 1.13x | 1.05x | 1.06x |

- Across random join orders, the max/min ratio is only 1.6x with RPT.
- Bloom filter operations are 28%, 12% and 46% of time on TPC-H, JOB and TPC-DS respectively.

**Other DuckDB TPC-H data points:**
- TPC-H SF100 on 96 cores 10.7s, SF1000 121s; Azure D64s v5 SF100 13.1s (arXiv 2506.09226) [snippet].
- DuckDB 1.5.0 (2026-03-09): TPC-H SF100 throughput score rose from 246,115.60 to 287,122.97 (1.4 to 1.5, their internal harness).
- DuckDB 1.4 LTS post (https://duckdb.org/2025/10/09/benchmark-results-14-lts): TPC-H SF100,000 on i8g.48xlarge, median 1.19h per query.
- Raspberry Pi 5 with NVMe (https://duckdb.org/2025/01/17/raspberryi-pi-tpch): SF100 geomean 11.7s / total 372.3s; SF300 geomean 55.2s / total 1561.8s.
- DuckDB 2.0 async I/O: TPC-H Q6 SF100 on S3 went from 8.23s to 2.84s [snippet via MotherDuck].
- Sirius (GPU DuckDB extension) is about 7x DuckDB at SF100 [snippet].

**ClickHouse:**
- Joins blog (https://clickhouse.com/blog/clickhouse-fast-joins): TPC-H SF100 got 26x faster from 22.4 to 26.4. ClickHouse Cloud SF100 19.8s [snippet].
- Exasol's critique: ClickHouse 26.4 has a median 5.8x slower than Exasol on TPC-H; Q21 76.7s [snippet].
- ClickHouse remains weak on multi-join TPC-H relative to Umbra and CedarDB, and strong on single-table scans.

### 2.2 Official audited TPC-H

- The top result at SF1000 is Dell with 5,489,326 QphH@1000GB (2026-06-23) [snippet]. See https://www.tpc.org/tpch/results/tpch_perf_results5.asp.
- Audited results need ACID, refresh functions (RF1/RF2), a power test and a throughput test with concurrent streams, and price/performance. No embedded engine competes there.
- "Winning TPC-H" for rudb in practice means the power-run style sum (or geomean) of the 22 queries against DuckDB, Umbra and CedarDB at SF1-SF100 on the same machine. It does not mean the official QphH list.

### 2.3 Where rudb stands (see section 8)

- SF1, single thread, quiet machine: DuckDB/rudb total ratio 0.93-0.98x, i.e. near parity, slightly ahead.
- rudb executes 1.51x more instructions but runs about 1.6x higher IPC.
- The 10x target at SF1 means cutting to about 1.79G instructions from 27.27G, a 15x instruction reduction.
- Issue #770 "G10: Ten times DuckDB on TPC-H SF100" has these exit criteria: no query slower than DuckDB, plus an ablation for every layer.

## 3. TPC-DS

Published numbers are sparse. What exists:
- **DuckDB on a MacBook Neo** (https://duckdb.org/2026/03/11/big-data-on-the-cheapest-macbook):
  - TPC-DS SF100: median query 1.63s, total 15.5 min.
  - SF300: median 6.90s, total 79 min, with **Q67 alone taking 51 min** (spilling / big sort-aggregate).
  - ClickBench hot 54.27s on the same laptop.
- **RPT** (SIGMOD'25): 1.56x over DuckDB on TPC-DS (single thread). Bloom operations are 46% of TPC-DS time, the heaviest of any workload measured.
- No public DuckDB vs Umbra/CedarDB TPC-DS totals were found. Umbra and CedarDB do not publish TPC-DS totals.
- The official TPC-DS list is audited Spark and cloud vendor results with the same audit regime as TPC-H. It is irrelevant to an embedded engine.
- TPC-DS stresses:
  - optimizer breadth: 99 query templates with correlated subqueries, CTEs, ROLLUP/GROUPING SETS, window functions, INTERSECT/EXCEPT, and many star joins;
  - outliers: Q67, Q72, Q4, Q11, Q14, Q23, Q78 dominate totals;
  - spilling at larger SFs.
- DuckDB 1.4 made CTEs materialized by default. DuckDB 2.0 added aggregate spilling and partial-aggregate pushdown below joins. All of these target TPC-DS-like shapes.

## 4. JOB (Join Order Benchmark) and CEB

### 4.1 Totals

[computed] from /tmp/diamond (umbra-db/diamond-vldb2024), sums of per-query median times:

| workload | system | queries | exec sum | compile sum |
|---|---|---|---|---|
| JOB | DuckDB v0.9 | 113 | 18.210s | n/a |
| JOB | Umbra base, 32 threads | 113 | 0.928s | 7.592s |
| JOB | Umbra + lookup and filter variant | 113 | 0.870s | ~88s |
| JOB | Umbra single thread | 113 | 7.756s | - |
| CE (CEB-style, 2975 queries) | DuckDB v0.9 | 2975 | 6721s | n/a |
| CE | Umbra base | 2975 | 174.97s | 141.8s |

- On JOB, **Umbra's compile time is about 8x its execution time**. End to end, Umbra is about 8.5s vs DuckDB 18.2s, only about 2.1x [derived], unless compile is hidden by adaptive execution.
- The DuckDB numbers are from v0.9 (2023). DuckDB's join-order optimizer and filter pushdown have improved since, so current DuckDB JOB is likely lower. No current published total was found.
- Bespoke OLAP, CEB: single-thread SF2 is 45.33x DuckDB and 9.56x Umbra. Multi-thread SF5: DuckDB 14.7s, Umbra 1.1s, Bespoke 0.6s.
- RPT: 1.46x over DuckDB on JOB, single thread.

### 4.2 What JOB and CEB measure

- JOB: 113 queries over IMDB (about 3.6GB CSV), 3-16 joins per query, correlated predicates, skew. It was designed to break cardinality estimation.
- CEB: thousands of queries from templates over IMDB. Per-query times are small, so plan quality, per-query fixed overhead (parse, optimize, compile) and robustness to bad join orders dominate.
- For a compiling engine, **JOB is the worst case for compile latency**: many short queries with large plans. Umbra's 7.6s of compile vs 0.93s of execution shows it.

## 5. TPC-C and OLTP in embedded analytical engines

### 5.1 DuckDB

- No published TPC-C or TPC-C-like numbers from DuckDB Labs.
- The DuckDB docs ("performance guide", "concurrency") say that many small concurrent writes and queries are not a design goal. It uses optimistic MVCC, and concurrent writers to the same rows conflict.
- Relevant engine facts:
  - Prepared statements help for queries under about 100ms, by avoiding parse and plan.
  - ART index lookups are tuple-at-a-time.
  - An UPDATE on an indexed column is executed as DELETE plus INSERT.
  - The vectorized engine has a fixed per-query overhead (pipeline setup, 2048-row vectors) that dominates single-row transactions.
- DuckDB 1.5.0: non-blocking checkpointing (writes are no longer stalled by the checkpoint).
- DuckDB 2.0: buffer-managed ART.

### 5.2 Compiled and specialized OLTP reference numbers

| system | source | config | throughput |
|---|---|---|---|
| HyPer | ICDE 2011 (Kemper & Neumann) | 12 warehouses, stored procedures, logical redo log, 1 OLTP thread | new-order 56,961 tps, total 126,576 tps |
| HyPer | same | 1 OLTP thread + 8 concurrent OLAP query streams | new-order 29,359, total 65,269 |
| HyPer | same | 5 OLTP threads (partitioned) | new-order 171,384, total 380,868 |
| VoltDB | cited in HyPer paper | single node / 6 nodes | 55,000 / 300,000 tps |
| Umbra MVCC | PVLDB 15, Freitag et al. (https://www.vldb.org/pvldb/vol15/p2797-freitag.pdf) | TPCC 100 warehouses, Xeon Gold 6212U 24c/48t, UmbraScript procedures, warehouse hash-partitioned | 1 thread 27,000 TX/s; max at 48 threads 413,300 TX/s |
| PostgreSQL | same paper | PL/pgSQL | 1 thread 2,600; max 44,700 |
| DBMS AD (disk commercial) | same | | 1 thread 1,100; peak 14,900 at 24 threads, then drops to ~9,000 |
| DBMS AM (in-memory commercial) | same | | 1 thread 4,000; max 22,000 |
| LeanStore standalone | cited in Umbra MVCC paper | hard-coded driver, no concurrency control, same hardware | 1 thread 41,000; 857,000 at 48 threads |
| LeanStore | VLDB'23 [snippet] | | 67K tps 1 thread; 845K on 10 cores; >3M tps with 128 threads |
| Silo | SOSP'13 [snippet] | 32 cores | ~700K tps |
| Hekaton | SIGMOD'13 | 12 cores, TPC-C-like | SQL Server 2,312 tps; interop 7,709; native compiled 36,375 (15.7x) |
| CedarDB | https://cedardb.com/blog/colibri/ | 64 clients | "3x Postgres" TPC-C throughput, no absolute numbers |

TATP in the Umbra MVCC paper:

| system | 1 thread | max at 48 threads |
|---|---|---|
| Umbra | 183,000 TX/s | 3,247,000 |
| PostgreSQL | - | 618,700 |
| DBMS AM | - | 237,900 |
| DBMS AD | - | 117,700 (at 40 threads) |

Umbra MVCC ablation, TPCC in thousands of TX/s:

| configuration | 1 client | 24 clients |
|---|---|---|
| non-transactional Umbra | 32.8 | 452.0 |
| + transaction lists | 31.6 (-1.04x) | 440.7 (-1.03x) |
| + shared writer latches | 31.3 | 429.8 |
| + snapshot isolation | 27.0 (-1.22x) | 366.0 (-1.23x) |
| - in-place updates | 6.4 (-5.13x) | 86.2 (-5.24x) |

- **In-place updates are worth about 5x.** An UPDATE-as-DELETE+INSERT design, which is DuckDB's style for indexed columns, pays that.
- The paper notes Umbra loses to LeanStore partly because Umbra supports only non-clustered relations. That roughly doubles index lookups. LeanStore's driver is also hard-coded.

Hekaton (SIGMOD'13) compiled vs interpreted CPU cycles:
- Lookups: 10.8x at 1 lookup per transaction, about 20x at 10 or more.
- Updates: 20-31x.
- Quote: "To go 10X faster, the engine must execute 90% fewer instructions."

Official audited TPC-C (distributed, irrelevant for embedded but defines "record") [snippet]:
- PolarDB 2.055B tpmC (2025)
- TDSQL about 814M tpmC
- OceanBase 707M tpmC

### 5.3 What matters for TPC-C in an embedded engine

- **Per-transaction instruction count.** A new-order is about 10 index lookups, 10+ inserts and a few updates.
  - With parse, plan and executor setup per statement, a vectorized engine spends 10^5-10^6 instructions per statement on overhead.
  - Compiled procedures (HyPer, Hekaton, UmbraScript) bring a transaction down to roughly 10^4 instructions.
- **Plan and compile caching is mandatory.** Compiling per statement is fatal: 50-100ms CedarDB compile vs a microsecond-scale transaction.
- **Index structure.** Point lookups need an ART or B-tree with an O(1)-ish probe. Clustered or primary storage avoids the double lookup. Range scans are needed for order-status and delivery.
- **In-place updates versus version chains.** 5x in the Umbra ablation.
- **Concurrency control.** Snapshot isolation costs 1.22x at one thread in Umbra. Latch-free structures, or partitioning by warehouse, are needed to scale.
- **Logging and group commit.** fsync per commit caps throughput at the device's fsync rate, so group commit or a log buffer is required. HyPer used logical redo.
- **Row versus column storage.** Columnar insert and update of 10-20 columns per row touches 10-20 cache lines. Row-group append buffers or a delta store help.

## 6. DuckDB 1.4 / 1.5 / 2.0 changes that affect the comparison

### DuckDB 1.4.0 "Andium" LTS (2025-09-16, https://duckdb.org/2025/09/16/announcing-duckdb-140)

- New k-way merge sort. lineitem sort at SF100 went from 273.98s to 80.92s (https://duckdb.org/2025/09/24/sorting-again).
- CTEs materialized by default.
- In-memory table compression (5-10x less memory).
- Database encryption, MERGE INTO, Iceberg writes.
- LTS; the 1.4 benchmark post above claims ClickBench in-memory #1.

### DuckDB 1.5.x (1.5.0 on 2026-03-09 through 1.5.5 on 2026-07-22)

- Non-blocking checkpointing.
- TPC-H SF100 throughput score from 246,115.60 to 287,122.97.
- rudb's 2026-09-23 comparisons use 1.5.5. The c6a.4xlarge board entry (2026-05-11) is likely 1.5.2.

### DuckDB 2.0 (alpha 2026-09-02, https://duckdb.org/2026/09/02/try-duckdb-20-alpha; highlights https://duckdb.org/2026/08/17/duckdb-20-highlights)

- Partial-aggregate pushdown below joins (TPC-H/DS style group-by over join).
- Recursive CTE rewrite: example 4.90s to 0.12s.
- Aggregate spilling (larger-than-memory group-by).
- Async I/O: TPC-H Q6 SF100 on S3 from 8.23s to 2.84s [snippet].
- Zone maps for more types and for functions of columns.
- Storage v2: DICT_FSST is the default string compression, and the ART is buffer-managed.
- PEG parser.
- Consequence: the ClickBench string queries (Q21-Q23, Q29, Q34/35) will likely move with DICT_FSST plus zone-map changes. rudb compares against v2.0.0-dev84237 in several reports, so the moving target is already partly included.

**The target moves.** A 10x claim must fix the DuckDB version. The rudb-bench README "ledger" explicitly flags when a rival's version changes between rows.

## 7. Benchmark critiques, 2025-2026

- **"Survivorship Bias" (Marcus et al., CIDR 2026 Best Paper, https://www.vldb.org/cidrdb/papers/2026/p22-marcus.pdf).**
  - The critique: DB research and marketing optimize for standard benchmarks (TPC-H, JOB, ClickBench) whose characteristics differ from real workloads.
  - Techniques that win benchmarks may not transfer.
  - Relevant as a warning against overfitting a compiler to 22 or 43 queries (cf. Bespoke OLAP).
- **QuestDB "Lies, damn lies and database benchmarks"** (see 1.5). Process lifetime, restarts and the baseline choice change ClickBench results by 1.2-2.2x.
- **Exasol vs ClickHouse TPC-H** [snippet]. A vendor critique showing ClickHouse's weakness on joins (median 5.8x slower, Q21 76.7s).
- **ClickBench's own changelog** is effectively a list of accepted critiques:
  - cold runs were not really cold (fixed 2026-05-11);
  - in-memory systems were unfairly compared on cold (fixed 2025-10-26);
  - there was no storage-tier distinction (fixed 2026-09-18).
- **LLM-generated engines** (gendb on ClickBench; Bespoke OLAP) raise the question of per-benchmark specialization. Bespoke's 11-45x over DuckDB is achieved by workload-specific storage and code, which is exactly what TPC rules forbid (no benchmark-special code paths).
- **Diamond hardware (VLDB 2024).** The same systems rank differently on different CPUs. This is a reminder to publish per-machine.

## 8. rudb-bench: local measured status (/Users/apple/github/tamnd/rudb-bench)

### 8.1 Machines

| name | hardware | notes |
|---|---|---|
| gamingpc-wsl / gpc | Intel i9-13900K, 32 logical CPUs, WSL2, 31 GiB | Hot runs are about 1.84-1.93x faster than c6a.4xlarge board numbers; cold runs are invalid under WSL2 (no real cache drop) |
| server2 | 6 threads | quiet machine used for TPC-H per-core runs |
| server3 | - | was loaded; its 0.85x TPC-H ratio is superseded |
| vmi3391933 | 8 threads | not comparable with gamingpc (reporting rule 7) |
| MacBook Air M4 | - | the 901x TPC-H figure from 2026-09-18 is stale (rudb then read Parquet) |

### 8.2 ClickBench baselines from README.md (board, c6a.4xlarge, recomputed 10 Sep 2026)

- Umbra 8.10s, ClickHouse 18.07s, DuckDB 26.25s, Polars 45.35s, DataFusion 45.57s, Velox 81.51s.
- 10x DuckDB = 2.63s.
- Umbra's 8.10 has since become 7.41 with the 2026-09-18 result.

Historical: rudb 0.3.5 vs DuckDB v2.0.0-dev84237 on gamingpc-wsl (32 threads), 41 shared queries, strided samples of hits:

| | 1m rows | 10m rows |
|---|---|---|
| DuckDB | 556ms | 3.015s |
| rudb | 5.854s | 51.929s |
| rudb/DuckDB | 11.28x slower | 19.51x slower |
| cores used, DuckDB | 2.25 | 9.01 |
| cores used, rudb | 0.94 | 0.99 |

At that time rudb was effectively single-threaded.

First full ClickBench, gamingpc-wsl, real 100M-row hits.parquet (README "2a the baseline"):

| engine | hot total | hot cpu | peak RSS | load | on disk | vs DuckDB* |
|---|---|---|---|---|---|---|
| DuckDB | 25.932s | 313.590s | 10.82 GiB | 57.865s | 24.95 GiB | 1.00x |
| clickhouse-local | 25.619s | 393.530s | 7.64 GiB | 36.493s | 9.69 GiB | 1.32x |
| DataFusion | 23.716s | 555.330s | 10.83 GiB | no load | 13.76 GiB | 1.21x |
| Polars | 25.947s | 507.320s | 17.04 GiB | no load | 13.76 GiB | 1.48x |
| clickhouse-server | 10.329s | not read | not read | 206.791s | 8.77 GiB | 0.53x |

- *The ratio is over the 39 queries every column ran, not over totals.
- DuckDB's q29 alone is 7.293s of 25.932s.
- clickhouse-server vs clickhouse-local is a 2.48x gap for the same binary (sorting key plus a long-lived process).

### 8.3 reports/2026-09-18: full file, official driver (99,997,497 rows)

Files: the-full-file.md, official-driver-full-file.md.

| engine | version | load | size | cold | hot | answered |
|---|---|---|---|---|---|---|
| rudb | 0.3.33 | 324.297s | 43.0GB | 637.90s | 27.07s | 42/43 |
| DuckDB | v2.0.0-dev84237 | 40.834s | - | 15.87s | 14.03s | 43/43 |
| ClickHouse | 26.9.1.1162 | 86.995s | - | 12.63s | 9.43s | 43/43 |

- rudb Q6 `COUNT(DISTINCT SearchPhrase)` OOMs (issue #755).
- rudb is 1.93x DuckDB's hot time, with a 43GB format vs DuckDB's ~20GB on the board.
- Cold is 40x worse, but cold is invalid on WSL2.

### 8.4 reports/2026-09-23

**q22-late-phrase-fetch.md** (gpc, 16 threads, fresh process per run, 9 runs, DuckDB 1.5.5):

| rows | engine | SQL median sum | process wall | CPU | peak RSS |
|---|---|---|---|---|---|
| 1M | rudb | 0.228s | 0.475s | 1.42s | 138.3 MiB |
| 1M | DuckDB | 0.511s | 0.969s | 3.85s | 272.5 MiB |
| 10M | rudb | 0.836s | 1.102s | 8.41s | 577.6 MiB |
| 10M | DuckDB | 2.774s | 3.443s | 30.65s | 1774.8 MiB |

- The 10M lead is 3.13x by process wall (3.32x by SQL median). It is not yet 10x.

**native-gram-sieve.md**:
- Native format 28 adds 4-byte-gram signatures per 1,024-value dictionary block, to skip LIKE '%x%' blocks.
- Q21 instructions fell 17.58%.
- FSST decompression remains about 20% of Q21.

### 8.5 reports/2026-09-24 (all TPC-H SF1)

**where-tpch-actually-stands.md**:
- rudb 0.4.20 vs DuckDB v2.0.0-dev84237. Both finish 22/22 with matching answers.
- Files are 250.66 MiB (rudb) vs 266.51 MiB (DuckDB).
- Supersedes the earlier 0.85x (loaded server3) and the 901x (M4, Parquet-reading rudb).

**tpch-per-core-on-a-quiet-machine.md** (server2, single thread, 7 alternating rounds, two sessions):

| session | DuckDB total | rudb total | rudb/DuckDB |
|---|---|---|---|
| A | 6651 ms | 6194.9 ms | 0.93x |
| B | 6056 ms | 5920.3 ms | 0.98x |

Per query, rudb/DuckDB time across the two sessions:

| rudb slower | ratio | rudb faster | ratio |
|---|---|---|---|
| q01 | 1.34/1.42x | q19 | 0.49/0.57x |
| q13 | 1.32/1.38x | q11 | 0.61/0.63x |
| q21 | 1.28/1.38x | q14 | 0.62/0.73x |
| q12 | 1.25/1.30x | q08 | 0.67/0.71x |
| q09 | 1.09/1.08x | | |

**tpch-instructions-retired.md**:
- rudb retires 27.27G instructions vs DuckDB's 17.90G, i.e. 1.51x/1.52x.
- Its CPU time is 0.88-0.96x DuckDB's, so IPC is about 1.6x DuckDB's.
- Worst per-query instruction ratios:

| query | rudb/DuckDB instructions |
|---|---|
| q01 | 2.34x |
| q12 | 1.96x |
| q18 | 1.94x |
| q21 | 1.74x |
| q13 | 1.71x |

- q01 profile: bit-unpacking about 15%, memory movement about 15%, filter gather 4%.
- Startup: DuckDB 169.7M instructions vs rudb 37.2M.
- 10x less work than DuckDB means about 1.79G instructions, **a 15x reduction from rudb today**. This is the Hekaton "90% fewer instructions" rule, applied twice over.

**Issue #770 "G10: Ten times DuckDB on TPC-H SF100"**: no query slower than DuckDB, plus an ablation for every layer (the `rudb-bench attribute` command).

### 8.6 Takeaways from local data

- TPC-H: the interpreter-plus-format rudb is already at DuckDB parity per core, with higher IPC. The remaining gap to 10x is instruction count. q01 and q12 are the classic compile-friendly scan-filter-aggregate queries where fused compiled loops (no materialized vectors, no gather) pay the most.
- ClickBench full-file: 2x behind DuckDB hot, with memory (Q6 OOM), format size (43GB) and cold load as the blockers. None of these are compiler problems.
- The slice benchmarks show that string-skip layers (gram sieve, late phrase fetch) give the 3x wins on Q21/Q22.

## 9. What winning each benchmark requires

"Win" means: beat the best system on the relevant board or paper on the same hardware, and reach 10x DuckDB where physically plausible.

### 9.1 ClickBench (single table, 43 queries, hot/cold/combined)

- **Bottleneck type:**
  - String processing: regex (Q29), LIKE substring (Q21-Q23), strlen (Q28).
  - High-cardinality hash aggregation on string or wide keys (Q33-Q35, Q19, Q17, Q18).
  - COUNT DISTINCT memory.
  - For cold and combined: I/O volume, load time and on-disk size.
  - On metal: parallel scaling of the final merge phases.
- **Bar:**
  - Umbra hot 7.41s on c6a.4xlarge; 10x DuckDB is 2.63s.
  - Score #1 requires near-best on every query, including the 10ms ones (Umbra/CedarDB win many of those).
- **What a compiler helps:**
  - Fused scan-filter-aggregate loops for Q1-Q20 style numeric queries.
  - Specialized hash-table probe and insert with packed keys.
  - Predicate evaluation on dictionary codes or compressed data without a per-vector interpretation overhead.
  - Avoiding materialization between operators.
  - Constant-folding the regex or LIKE pattern into a specialized matcher, e.g. compiling `'%google%'` into a SIMD memmem or `REGEXP_REPLACE` domain extraction into a hand-written URL parser.
- **What it doesn't help:**
  - Evaluating the regex once per dictionary entry instead of per row. That is a storage and encoding decision.
  - Skipping data (zone maps, gram signatures, sorted keys).
  - On-disk size, load time, cold I/O.
  - Memory blow-up of COUNT DISTINCT (an algorithm or spill decision).
  - The per-query compile latency itself, which hurts the ~10ms queries under the geometric-mean score. A compiling engine must have a fast path (interpreter or bytecode) for short queries, as Umbra has, or it loses on score while winning the sum.
  - Methodology: process restart, the metric and the machine class.

### 9.2 TPC-H (8 tables, 22 queries, SF1-SF100+)

- **Bottleneck type:**
  - Hash join probe and build (memory latency bound).
  - Hash aggregation.
  - Scan and decompress bandwidth for lineitem (q01, q06, q12, q14, q19).
  - Semi and anti joins and correlated subqueries (q02, q04, q17, q20, q21, q22).
  - Join order and predicate transfer.
  - At SF1: fixed per-query overhead and compile time (Umbra's optimized compile is 1.10s vs 0.107s execution).
- **Bar:**
  - Umbra and CedarDB (CedarDB claims about 6x Hyper/DuckDB at SF100).
  - Bespoke OLAP shows the ceiling: 11x DuckDB single-threaded, 7.65x multi-threaded, with 12.35x attributed mostly to data-aware storage.
- **What a compiler helps:**
  - Tuple-at-a-time fused pipelines: the q01 profile's 15% memory movement plus 4% gather disappears.
  - Register-resident aggregates.
  - Specialized hash tables (about 10 instructions per lookup, as in CedarDB).
  - Dense-key and array aggregation when key ranges are known.
  - Bitmap semi-joins.
  - Inlined arithmetic on decimals.
  - It is the main lever for a 15x instruction reduction on q01/q12/q18.
- **What it doesn't help:**
  - Join order and cardinality estimation (the plan must be right first).
  - Predicate transfer and Bloom filter placement (an RPT-style plan-level 1.44x).
  - Sort order and zone maps (lineitem by shipdate).
  - Decimal representation choice.
  - Memory bandwidth at large SF once code is tight.
  - Compile latency at SF1. Adaptive execution is needed to not lose small-SF runs.
  - Official audited QphH (requires refresh functions, ACID, concurrency and pricing; not a target).

### 9.3 TPC-DS (24 tables, 99 queries)

- **Bottleneck type:**
  - Optimizer breadth and robustness: correlated subqueries, CTE reuse, ROLLUP/GROUPING SETS, windows, INTERSECT/EXCEPT, star joins with many dimensions.
  - Totals dominated by a few outliers (Q67 took 51 of 79 min at SF300 on a MacBook Neo).
  - Spilling (sort and aggregate beyond memory).
  - Bloom and filter work is 46% of time under RPT.
- **Bar:** there is no public Umbra or CedarDB total. DuckDB plus RPT is 1.56x DuckDB. Winning mostly means not having any query 10x slower than DuckDB.
- **What a compiler helps:** the per-operator efficiency of window functions and aggregations, star-join probe chains (multi-way probe fused in one loop), and Bloom-probe loops.
- **What it doesn't help:**
  - Unnesting and decorrelation.
  - CTE materialization decisions.
  - Grouping-sets planning.
  - Spilling algorithms.
  - Coverage of all 99 queries. Correctness and breadth dominate, and one missing feature equals a loss.

### 9.4 JOB / CEB (IMDB, 113 / thousands of multi-join queries)

- **Bottleneck type:**
  - Plan quality under bad cardinality estimates; robustness to join order (the RPT max/min ratio is 1.6x).
  - Per-query fixed overhead: parse, optimize and **compile**.
  - Index nested-loop joins on selective predicates.
  - Many small intermediate results.
- **Bar:**
  - Umbra JOB execution 0.93s at 32 threads vs DuckDB v0.9 18.2s.
  - CEB: Bespoke 0.6s vs Umbra 1.1s vs DuckDB 14.7s (SF5, multi-threaded).
- **What a compiler helps:** fused multi-join probe pipelines, semi-join bitmaps and cheap Bloom probes, and index-nested-loop code.
- **What it doesn't help:**
  - Compile latency is the enemy. Umbra spends 7.6s compiling vs 0.93s executing JOB, and a lookup and filter variant compiles for about 88s. An LLVM-per-query design loses JOB end-to-end.
  - Needed instead: a bytecode or interpreter tier, compile caching, and background compilation (Flying Start or adaptive).
  - Join enumeration and cardinality estimation.
  - Predicate transfer (RPT 1.46x).
  - Choosing index NLJ vs hash join.

### 9.5 TPC-C (and TPC-C-like OLTP in an embedded engine)

- **Bottleneck type:**
  - Instructions per transaction: statement dispatch, plan lookup, index probes, tuple insert and update, and MVCC bookkeeping.
  - Latching and contention.
  - Commit and log I/O (fsync).
  - Update-in-place vs version and delete+insert: 5.1x in Umbra's ablation.
- **Bar:**
  - DuckDB has no published TPC-C. Beating it is easy but uninteresting.
  - Meaningful bars, one thread: HyPer 126k tps (12 warehouses, no CC); Umbra 27k TX/s (100 warehouses, SI MVCC); LeanStore 41k-67k.
  - Scaled: Umbra 413k TX/s at 48 threads; Hekaton 15.7x over interpreted SQL Server.
- **What a compiler helps:**
  - Compiling whole transactions or stored procedures into native code: 10-30x fewer cycles per Hekaton.
  - Specialized index probe code per key type.
  - Removing per-statement interpretation overhead.
  - This only pays if compiled code is **cached and reused**, i.e. prepared statements or procedures compiled once, since a transaction lasts microseconds while compilation takes milliseconds.
- **What it doesn't help:**
  - Storage-engine design: row or clustered layout, in-place updates, version chains.
  - Concurrency control: SI costs 1.22x; partitioning by warehouse.
  - WAL and group commit.
  - B-tree/ART latching.
  - Single-writer embedding constraints.
  - A columnar analytical format with DuckDB-style UPDATE = DELETE+INSERT caps throughput regardless of codegen.

### 9.6 Cross-cutting conclusions for the rudb compiler spec

1. **A compiler is necessary but not sufficient for 10x.** Bespoke OLAP attributes most of its 11-45x to data-aware storage, and only 1.26x to flat storage. Codegen is the multiplier on top of the right layout and plan.
2. **Tiered execution is non-negotiable.** Compile latency loses JOB, CEB, small-SF TPC-H, the short ClickBench queries under the geometric-mean score, and TPC-C.
3. **Instruction count is the metric to gate on.** rudb must go from 1.51x DuckDB's instructions to 0.1x (a 15x reduction) on TPC-H. Wall time on a noisy machine is not enough (see the rudb-bench superseded ratios).
4. **ClickBench's top 5 queries are string or regex work plus high-cardinality GROUP BY.** Dictionary-level evaluation, string skipping and hash-table design matter more there than loop fusion.
5. **Fix the rival's version and the methodology** (DuckDB 1.5.5 vs 2.0-dev, restart rules, machine) in every claim.

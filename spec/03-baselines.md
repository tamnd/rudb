# Baselines: where the time and the bytes actually are

This document is the measurement the rest of the specification argues from. Everything in it was recomputed locally from public artifacts on 10 September 2026, not copied from a vendor summary page, and the commands are given so anyone can redo it. If a number here is wrong, documents 02, 05, 06 and 07 are aimed at the wrong thing.

## 3.1 How these numbers were produced

ClickBench publishes one JSON per system per date per instance type in [ClickHouse/ClickBench](https://github.com/ClickHouse/ClickBench), under `<system>/results/<date>/<instance>.json`. Each file has a `result` array of 43 entries, one per query, each entry being three timings. Run one is cold, runs two and three are hot.

```
curl -s https://raw.githubusercontent.com/ClickHouse/ClickBench/main/duckdb/results/20260511/c6a.4xlarge.json
curl -s https://raw.githubusercontent.com/ClickHouse/ClickBench/main/umbra/results/20260815/c6a.4xlarge.json
curl -s https://raw.githubusercontent.com/ClickHouse/ClickBench/main/clickhouse/results/20260909/c6a.4xlarge.json
```

For each query I take the minimum of runs two and three as the hot time and run one as the cold time, then sum across the 43 queries. That is not the official Combined metric, which is a weighted geometric mean of load time at 10 percent, data size at 10 percent, cold runtime at 20 percent and hot runtime at 60 percent. The sum is used here because it is the metric that tells you where engineering effort should go, and a geometric mean deliberately flattens exactly the concentration this document is about.

Each system's result was taken from its most recent submission that includes `c6a.4xlarge`, so the dates differ by system. DuckDB's is 2026-05-11, Umbra's is 2026-08-15, ClickHouse's is 2026-09-09. That is a real limitation and it is the reason document 15 requires `rudb-bench` to run every comparison itself on one machine on one day rather than quoting this board.

**The machine.** `c6a.4xlarge` is 16 vCPUs on an AMD EPYC 7R13, which is Zen 3 at up to 3.6 GHz, meaning 8 physical cores with SMT, plus 32 GiB of RAM and EBS gp2 storage. It is a small machine by 2026 standards and that matters: at 32 GiB the 20.46 GB DuckDB database roughly fits in page cache after a cold pass, which is why hot and cold differ by a factor of four.

**The data.** The `hits` table is 99,997,497 rows and 105 columns. In DuckDB's `create.sql` the type distribution is 48 `SMALLINT`, 26 `TEXT`, 19 `INTEGER`, 6 `BIGINT`, 3 `TIMESTAMP`, 1 `DATE`, 1 `VARCHAR(255)` and 1 `CHAR`. Fixed-width columns alone at their declared widths are 24.8 GB of raw values before any strings, which is a useful floor to keep in mind when reading the compression targets in section 3.5.

## 3.2 The system board

| System | Date | Hot total | Cold total | Load | On disk |
|---|---|---|---|---|---|
| Umbra | 2026-08-15 | 8.10 s | 49.79 s | 164 s | 8.30 GB |
| HeavyAI | 2026-05-17 | 9.48 s | 253.71 s | 1132 s | 50.87 GB |
| ClickHouse | 2026-09-09 | 18.07 s | 110.97 s | 219 s | 9.42 GB |
| Hyper | 2026-09-03 | 21.22 s | 140.33 s | 109 s | 8.81 GB |
| Arc | 2026-05-11 | 24.90 s | 117.07 s | 58 s | 14.78 GB |
| Ursa | 2026-05-09 | 26.15 s | 118.56 s | 336 s | 15.43 GB |
| **DuckDB** | 2026-05-11 | **26.25 s** | **115.19 s** | **126 s** | **20.46 GB** |
| CedarDB | 2026-08-15 | 28.99 s | 200.71 s | 187 s | 8.46 GB |
| chDB | 2026-08-16 | 31.04 s | 103.68 s | 556 s | 22.07 GB |
| pgrust | 2026-08-01 | 34.88 s | 134.15 s | 223 s | 17.56 GB |
| DuckDB + Vortex | 2026-05-11 | 40.99 s | 208.39 s | 141 s | 15.73 GB |
| Databend | 2026-05-11 | 42.31 s | 182.15 s | 398 s | 20.92 GB |
| StarRocks | 2026-09-07 | 44.47 s | 278.03 s | 489 s | 17.94 GB |
| Polars | 2026-08-24 | 45.35 s | 179.30 s | 10 s | 14.78 GB |
| DataFusion | 2026-08-20 | 45.57 s | 182.91 s | 10 s | 14.78 GB |
| Doris | 2026-05-10 | 48.43 s | 208.43 s | 205 s | 13.78 GB |
| Velox | 2026-08-30 | 81.51 s | 182.49 s | 11 s | 14.78 GB |
| DataFusion + Vortex | 2026-08-20 | 91.30 s | 232.42 s | 81 s | 15.27 GB |

The 10 second load times for Polars, DataFusion and Velox are not loads. Those systems query the 14.78 GB Parquet file in place, which is why their on-disk figure is identical and why their hot times are worse. That tradeoff is worth stating plainly: reading Parquet directly costs roughly 1.7x against a native format on this workload, and any lakehouse-first design pays it.

**The Vortex rows deserve a paragraph.** Vortex is the closest published system to what documents 05 and 06 propose: a Rust columnar format with FSST, ALP and FastLanes bit-packing, compute kernels over encoded data, and a stable format as of 0.36.0. Its own claims are 10 to 20x faster scans than Parquet. On this board, plugging it into DuckDB moves the total from 26.25 to 40.99 and plugging it into DataFusion moves 45.57 to 91.30. Both directions are worse, by 1.6x and 2.0x. That is the strongest available evidence that a good compressed format does not automatically produce a fast engine, and it is the specific failure mode documents 06.7 and 07.2 are written to avoid. See document 19 open question three.

## 3.3 The per-query table

DuckDB, Umbra and ClickHouse hot times in seconds, and the DuckDB to Umbra ratio, which is the working proxy for how much headroom DuckDB is leaving.

| Q | DuckDB | Umbra | ClickHouse | d/u | Shape |
|---|---|---|---|---|---|
| 0 | 0.018 | 0.008 | 0.001 | 2.2 | `COUNT(*)` |
| 1 | 0.041 | 0.004 | 0.001 | 10.2 | `COUNT(*) WHERE AdvEngineID <> 0` |
| 2 | 0.074 | 0.027 | 0.032 | 2.7 | three scalar aggregates |
| 3 | 0.086 | 0.026 | 0.040 | 3.3 | `AVG(UserID)` |
| 4 | 0.348 | 0.131 | 0.219 | 2.7 | `COUNT(DISTINCT UserID)` |
| 5 | 0.338 | 0.174 | 0.403 | 1.9 | `COUNT(DISTINCT SearchPhrase)` |
| 6 | 0.030 | 0.024 | 0.009 | 1.2 | `MIN/MAX(EventDate)` |
| 7 | 0.038 | 0.004 | 0.009 | 9.5 | tiny group-by with selective filter |
| 8 | 0.443 | 0.160 | 0.490 | 2.8 | `COUNT(DISTINCT UserID) GROUP BY RegionID` |
| 9 | 0.612 | 0.227 | 0.552 | 2.7 | five aggregates `GROUP BY RegionID` |
| 10 | 0.150 | 0.025 | 0.135 | 6.0 | distinct-count group-by, selective |
| 11 | 0.170 | 0.028 | 0.167 | 6.1 | two-key version of Q10 |
| 12 | 0.408 | 0.161 | 0.403 | 2.5 | `GROUP BY SearchPhrase` |
| 13 | 0.781 | 0.309 | 0.585 | 2.5 | `COUNT(DISTINCT UserID) GROUP BY SearchPhrase` |
| 14 | 0.473 | 0.183 | 0.463 | 2.6 | two-key group-by |
| 15 | 0.389 | 0.171 | 0.289 | 2.3 | `GROUP BY UserID` top-10 |
| 16 | 0.892 | 0.368 | 1.130 | 2.4 | `GROUP BY UserID, SearchPhrase` top-10 |
| 17 | 0.659 | 0.215 | 0.462 | 3.1 | same, plain `LIMIT` |
| 18 | 1.650 | 0.846 | 2.131 | 2.0 | `GROUP BY UserID, minute(EventTime), SearchPhrase` |
| 19 | 0.049 | 0.002 | 0.002 | 24.5 | point lookup on `UserID` |
| 20 | 0.711 | 0.133 | 0.375 | 5.3 | `COUNT(*) WHERE URL LIKE '%google%'` |
| 21 | 0.769 | 0.055 | 0.102 | 14.0 | Q20 plus group-by and `MIN(URL)` |
| 22 | 1.169 | 0.064 | 0.559 | 18.3 | Q21 plus `Title LIKE` and distinct-count |
| 23 | 0.349 | 0.024 | 0.102 | 14.5 | `SELECT *` with `LIKE` filter, top-10 |
| 24 | 0.072 | 0.005 | 0.087 | 14.4 | top-10 by `EventTime` |
| 25 | 0.166 | 0.010 | 0.195 | 16.6 | top-10 by `SearchPhrase` |
| 26 | 0.069 | 0.004 | 0.071 | 17.2 | top-10 by two keys |
| 27 | 0.645 | 0.113 | 0.162 | 5.7 | `AVG(STRLEN(URL)) GROUP BY CounterID` |
| 28 | **6.478** | **1.393** | 1.649 | 4.7 | `REGEXP_REPLACE` over `Referer` |
| 29 | 0.068 | 0.030 | 0.036 | 2.3 | ninety sums over a 16-bit column |
| 30 | 0.407 | 0.084 | 0.266 | 4.8 | `GROUP BY SearchEngineID, ClientIP` |
| 31 | 0.611 | 0.129 | 0.340 | 4.7 | `GROUP BY WatchID, ClientIP` filtered |
| 32 | **2.035** | **1.323** | 2.177 | 1.5 | `GROUP BY WatchID, ClientIP` unfiltered |
| 33 | **2.054** | **0.730** | 2.042 | 2.8 | `GROUP BY URL` top-10 |
| 34 | **2.198** | **0.732** | 2.014 | 3.0 | `GROUP BY 1, URL` top-10 |
| 35 | 0.468 | 0.123 | 0.204 | 3.8 | `GROUP BY ClientIP` and three derived keys |
| 36 | 0.052 | 0.011 | 0.033 | 4.7 | date-ranged group-by on `URL` |
| 37 | 0.039 | 0.005 | 0.017 | 7.8 | date-ranged group-by on `Title` |
| 38 | 0.039 | 0.003 | 0.019 | 13.0 | date-ranged, `IsLink` filter |
| 39 | 0.086 | 0.022 | 0.071 | 3.9 | `CASE` expression group-by |
| 40 | 0.041 | 0.003 | 0.011 | 13.7 | `GROUP BY URLHash, EventDate` |
| 41 | 0.038 | 0.003 | 0.010 | 12.7 | window dimensions group-by |
| 42 | 0.039 | 0.005 | 0.008 | 7.8 | `DATE_TRUNC('minute', EventTime)` group-by |
| | **26.25** | **8.10** | **18.07** | median 4.65 | |

## 3.4 What the table says

**DuckDB's time is concentrated and Umbra's is more so.** The top sixteen queries by DuckDB time are 84.6 percent of DuckDB's total and 85.7 percent of Umbra's, so both engines are slow on the same things. The single query Q28 is 24.7 percent of DuckDB's total and 17.2 percent of Umbra's.

**Thirty-nine of forty-three queries have DuckDB at least 2x off Umbra**, covering 84.6 percent of DuckDB's time. Twenty-seven are at least 3x off, covering 59.6 percent. So DuckDB is leaving a great deal on the table almost everywhere, and the first factor of three is a matter of building a state-of-the-art engine rather than of inventing anything.

**Matching Umbra on every query gives 8.10 seconds, which is 3.2x.** This is the number that sets the difficulty of the whole project. Everything past 3.2x has to come from doing something Umbra does not do.

**Umbra's remaining time is concentrated in seven queries totalling 5.70 seconds, or 70.4 percent.** Q28 at 1.393, Q32 at 1.323, Q18 at 0.846, Q34 at 0.732, Q33 at 0.730, Q16 at 0.368, Q13 at 0.309. Document 02.4 works through what each one needs.

**The `LIKE '%google%'` family, Q20 through Q23 and Q27, is where DuckDB is worst in relative terms**, at 5.3x, 14.0x, 18.3x, 14.5x and 5.7x. Q22 at 18.3x is the largest gap of any substantial query. These are substring searches over the `URL` and `Title` columns, and the reason a good engine crushes them is that FSST-compressed strings support substring search on the compressed representation for a large class of patterns, so the scan never decompresses. That is not a subtle optimization, it is the difference between touching 1 GB and touching 6 GB, and it is the clearest single demonstration of the thesis in document 02.2 anywhere on this board.

**The top-N family, Q24 through Q26 and Q19, is where the ratios are largest in relative terms and smallest in absolute terms**, at 14.4x, 16.6x, 17.2x and 24.5x on 0.072, 0.166, 0.069 and 0.049 seconds. These are queries where an index or a sort order answers the question directly and a full scan does not. They contribute 0.36 seconds to DuckDB's total, so they are worth 0.34 seconds to us, which is 1.3 percent. They are worth fixing because they are easy and because they are embarrassing, not because they move the number.

**Q32 is the query where Umbra is least ahead, at 1.5x.** `GROUP BY WatchID, ClientIP` over 100 million rows with `WatchID` near-unique produces roughly 100 million groups, and both engines solve it by building a 100-million-entry hash table. Neither has a better idea. That is the query where document 07.5's heavy-hitter mechanism has the most room, and it is 16.3 percent of Umbra's total.

## 3.5 The byte budget

DuckDB stores this table in 20.46 GB, Umbra in 8.30, ClickHouse in 9.42, Parquet in 14.78. The axis-4 target is 2.05 GB.

The fixed-width columns at declared width are 24.8 GB: 48 `SMALLINT` at 2 bytes is 9.6 GB, 19 `INTEGER` at 4 bytes is 7.6 GB, 6 `BIGINT` at 8 bytes is 4.8 GB, 3 `TIMESTAMP` at 8 bytes is 2.4 GB, and the `DATE` is 0.4 GB. So getting the whole table to 2.05 GB requires the fixed-width half alone to compress by more than 12x before a single string byte is accounted for.

That is less absurd than it sounds, and the reason is the shape of the 48 `SMALLINT` columns. In the hits schema these are almost all flags and small enumerations: `IsRefresh`, `IsMobile`, `IsLink`, `IsDownload`, `IsNotBounce`, `IsEvent`, `IsParameter`, `SocialSourceNetworkID`, `SilverlightVersion1` through `4`, `CodeVersion`, and so on. Columns whose entire domain is zero and one, stored at 2 bytes each, are 9.6 GB of nearly nothing. Under run-length encoding cascaded into bit packing, most of them go to a few megabytes. Getting the fixed-width half from 24.8 GB to under 1 GB is a normal outcome for a good encoder, not an ambitious one.

The hard half is the strings, and specifically `URL`, `Referer` and `Title`. The plan in document 06 is FSST for the symbol-level redundancy, a global dictionary for the value-level redundancy, and multi-column compression to exploit the fact that `Referer` values are drawn from largely the same universe as `URL` values and that `Title` correlates strongly with `URL`. On top of that, `URLHash` and `RefererHash` are 1.6 GB of `BIGINT` that are pure functions of `URL` and `Referer` and can be stored as recomputation rules instead of as bytes, which document 06.6 covers.

**The honest uncertainty.** I have not measured the actual distinct counts of `URL`, `Referer` and `Title` in this dataset, nor the actual correlation between them, and the entire disk claim turns on those three numbers. That measurement is milestone M1's first task and document 19 open question one. If the distinct counts are large and the correlations weak, the number lands around 4 to 5 GB, which is 4 to 5x under DuckDB and roughly Umbra parity, and the 10x resource claim is wrong.

## 3.6 The other suites

ClickBench is one workload and optimizing only for it produces an engine that is good at one workload. `rudb-bench` therefore carries four more, and document 15 sets the rules for all of them.

**TPC-H at SF100.** Join-dominated, well understood, and the suite Bespoke OLAP measured 11.17x on against DuckDB 1.4.1 single-threaded and 7.65x at 64 threads. That gap between single-threaded and 64-threaded is itself a finding: roughly a third of their win does not survive parallelism, which sets a realistic expectation for how much of a layout-specialization win transfers to a many-core machine.

**TPC-DS at SF100.** Ninety-nine queries, far more diverse than TPC-H, with correlated subqueries, rollups and heavy use of the date dimension. It is the suite that punishes an engine with a narrow optimizer, and it is where document 09's rewrite set gets tested.

**JOB and CEB.** The Join Order Benchmark on the IMDB dataset and the Cardinality Estimation Benchmark that extends it. These are the suites where the difference between a good plan and a bad plan is three orders of magnitude, where cardinality estimation error dominates everything else, and where document 09's Robust Predicate Transfer either works or does not. Bespoke OLAP measured 45.33x on CEB single-threaded, and the ablation showed their flat-storage variant at 0.57x, meaning it lost to DuckDB. That contrast is the sharpest evidence in the literature that on join workloads the layout is the whole game.

**The H2O.ai group-by and join suite.** Small, fast, widely quoted in the dataframe community, and a good regression canary because it runs in seconds.

**Micro-benchmarks.** Per-operator, per-encoding, per-kernel, run on every commit. These are for diagnosis, not for headlines, and document 15 is explicit that a micro-benchmark number never appears in a public claim without the end-to-end number next to it.

## 3.7 What this document commits us to

Three things get carried forward as facts rather than re-derived.

Matching the current state of the art gets 3.2x, so the project's distinctive content is entirely in the gap between 3.2x and 10x, and that gap lives in seven ClickBench queries and three techniques.

The Rust ecosystem's current best on this workload is 45 seconds against DuckDB's 26, so the first phase of work is catch-up and should be planned and staffed as catch-up rather than as innovation.

The resource claim turns on three unmeasured numbers about the hits dataset, and measuring them is the first thing M1 does.

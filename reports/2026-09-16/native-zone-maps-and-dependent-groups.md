# Native zone maps and dependent group keys

The 1 million row profile showed two separate costs. Queries 37 through 40 decoded every projected
native page before their selective filters ran. Query 36 hashed four integer group keys even though
three keys were arithmetic expressions of `ClientIP`.

The native v4 directory now persists the minimum, maximum, and null count for each column page.
Catalog scans use those bounds through the same predicate pruning contract as in-memory tables.
The optimizer also removes deterministic group keys derived entirely from bare group keys and
computes them once per output group.

All values below are hot medians from fresh processes over the same 1 million row ClickBench input.
The rudb before and after runs use the same benchmark harness and six attempts per query.

| Query | DuckDB native | rudb before | rudb after | Change from before |
| --- | ---: | ---: | ---: | ---: |
| 36 | 10.0 ms | 26.6 ms | 16.5 ms | 37.8% faster |
| 37 | 5.0 ms | 12.1 ms | 3.4 ms | 72.2% faster |
| 38 | 4.0 ms | 13.4 ms | 2.9 ms | 78.1% faster |
| 39 | 4.0 ms | 12.4 ms | 3.2 ms | 74.2% faster |
| 40 | 7.0 ms | 22.2 ms | 7.3 ms | 67.0% faster |

| Engine | Query timer sum | Process wall sum | CPU sum | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| DuckDB native | 0.549 s | 1.465 s | 3.798 s | 309.6 MiB |
| rudb native before | 0.769 s | 0.938 s | 6.531 s | 180.9 MiB |
| rudb native after | 0.726 s | 1.256 s | 6.086 s | 203.6 MiB |
| DuckDB Parquet | 1.617 s | 3.274 s | 8.021 s | 360.8 MiB |
| rudb Parquet | 0.928 s | 1.179 s | 5.122 s | 191.9 MiB |

The native load still uses far less memory than DuckDB, while persisted statistics add measurable
work and a small amount of file space.

| Engine | Load wall | Load CPU | Peak RSS | File bytes |
| --- | ---: | ---: | ---: | ---: |
| DuckDB native | 3.578 s | 7.183 s | 2008.0 MiB | 281,817,088 |
| rudb native v4 | 2.762 s | 2.678 s | 45.3 MiB | 561,062,577 |

The full audit completed all 43 queries. Query 29 matched. The deterministic retest verifier matched
all queries it rewrote to resolve unordered limits and aggregate ties, including Query 40. The
aggregate query total remains 1.32 times DuckDB, so this change removes two known structural costs
without meeting the 10x target.

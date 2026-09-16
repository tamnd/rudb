# Native page checksum throughput

Native scans validated every requested page with a byte-at-a-time FNV checksum. Profiling Query 18
showed that checksum work consumed a material part of a scan that otherwise needed little execution
work. The checksum now processes four independent 64-bit lanes in each 32-byte iteration and uses a
64-bit avalanche before storing the result.

The checksum field remains eight bytes and readers still validate the committed directory and every
page they read. Because checksum values changed, the native format version advances from v4 to v5.
Existing v4 files must be loaded again.

The measurements below use the complete four-way 1 million row audit. Every query ran in a fresh
process once for the first measurement and five times for the hot median.

| Engine | Query timer sum | Process wall sum | CPU sum | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| DuckDB native | 0.542 s | 1.458 s | 3.811 s | 312.2 MiB |
| rudb native before | 0.679 s | 1.213 s | 5.255 s | 202.7 MiB |
| rudb native after | 0.615 s | 0.965 s | 4.136 s | 200.4 MiB |
| DuckDB Parquet | 1.618 s | 3.263 s | 7.965 s | 362.6 MiB |
| rudb Parquet | 0.920 s | 1.169 s | 5.000 s | 190.0 MiB |

| Native load | Wall time | CPU time | Peak RSS | File size |
| --- | ---: | ---: | ---: | ---: |
| DuckDB | 3.438 s | 8.969 s | 1693.3 MiB | 268.8 MiB |
| rudb before | 2.670 s | 2.577 s | 45.0 MiB | 535.1 MiB |
| rudb after | 2.341 s | 2.258 s | 44.8 MiB | 535.1 MiB |

| Native query | DuckDB | rudb before | rudb after | Change from rudb before |
| --- | ---: | ---: | ---: | ---: |
| Query 18 | 16.0 ms | 42.6 ms | 29.7 ms | 30.3% faster |
| Query 22 | 19.0 ms | 13.5 ms | 8.6 ms | 35.7% faster |
| Query 23 | 24.0 ms | 25.2 ms | 15.7 ms | 37.5% faster |
| Query 24 | 41.0 ms | 20.5 ms | 10.5 ms | 48.8% faster |
| Query 34 | 26.0 ms | 62.4 ms | 60.5 ms | 2.9% faster |
| Query 35 | 28.0 ms | 61.4 ms | 58.2 ms | 5.3% faster |

All 43 queries completed. Query 29 matched in the full audit, and all deterministic rewritten checks
passed. The native timer sum is now 1.13 times DuckDB. Queries 34 and 35 changed little because their
cost is high-cardinality string aggregation rather than page validation. The 10x target remains open.

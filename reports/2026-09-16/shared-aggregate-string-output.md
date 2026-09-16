# Shared aggregate string output

Callgrind on ClickBench Query 34 attributed 20.0 percent of executed instructions to `memcpy` and
9.15 percent to vector cloning. A grouped aggregate built flat string result chunks, and TopN cloned
those chunks while retaining candidate rows. Cloning a flat string vector copied its complete arena,
including strings TopN would later discard.

String key columns now move into the existing shared string-view representation when the aggregate
finishes a result chunk. Construction remains mutable and unchanged. Downstream operators clone a
page handle and the views instead of copying the string payload.

The measurements below use the complete four-way 1 million row audit. Every query ran in a fresh
process once for the first measurement and five times for the hot median.

| Engine | Query timer sum | Process wall sum | CPU sum | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| DuckDB native | 0.542 s | 1.460 s | 3.818 s | 308.2 MiB |
| rudb native before | 0.615 s | 0.965 s | 4.136 s | 200.4 MiB |
| rudb native after | 0.590 s | 0.938 s | 4.086 s | 202.1 MiB |
| DuckDB Parquet | 1.635 s | 3.275 s | 8.285 s | 365.7 MiB |
| rudb Parquet before | 0.920 s | 1.169 s | 5.000 s | 190.0 MiB |
| rudb Parquet after | 0.900 s | 1.146 s | 5.097 s | 192.5 MiB |

| Query | DuckDB native | rudb native before | rudb native after | DuckDB Parquet | rudb Parquet after |
| --- | ---: | ---: | ---: | ---: | ---: |
| Query 34 | 26.0 ms | 60.5 ms | 47.8 ms | 51.0 ms | 47.6 ms |
| Query 35 | 26.0 ms | 58.2 ms | 49.3 ms | 59.0 ms | 48.1 ms |

Native Query 34 improved by 21.0 percent and Query 35 by 15.2 percent. The complete native timer sum
improved by 4.1 percent. All 43 queries completed, Query 29 matched, and every deterministic rewritten
check passed. Native remains 1.09 times DuckDB by query time, so the 10x target remains open.

| Native load | Wall time | CPU time | Peak RSS |
| --- | ---: | ---: | ---: |
| DuckDB | 3.505 s | 7.237 s | 1932.7 MiB |
| rudb | 2.358 s | 2.276 s | 45.7 MiB |

The next controlling cost in Queries 34 and 35 is building and merging the high-cardinality string
hash tables. Reducing result copies does not remove that state.

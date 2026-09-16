# Native late materialization

ClickBench Query 24 selects all 105 columns, filters on `URL`, orders by `EventTime`, and returns ten
rows. The old native plan decoded every selected column for all one million input rows before TopN
discarded nearly all of them. Its median engine time was 76.3 ms and its median CPU time was 1.03
seconds.

Catalog scans can now append a table-wide row ordinal. The late materialization optimizer carries
that ordinal through the filter and TopN, then `TableFetch` reads the complete winning rows from the
native table. The table fetch preserves TopN order and works across memory chunks and native
stripes.

The measurements below use the full four-way 1 million row audit with six fresh processes per
query.

| Query 24 engine | Query median | Process wall median | CPU median | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| DuckDB native | 43.0 ms | 64.5 ms | 181.1 ms | 207.8 MiB |
| rudb native before | 76.3 ms | 91.4 ms | 1030.1 ms | 79.3 MiB |
| rudb native after | 20.5 ms | 33.0 ms | 169.1 ms | 33.3 MiB |
| DuckDB Parquet | 83.0 ms | 126.4 ms | 997.1 ms | 276.4 MiB |
| rudb Parquet | 27.3 ms | 33.1 ms | 204.8 ms | 62.2 MiB |

Native Query 24 is 73.1 percent faster than the previous native plan and 2.09 times faster than
DuckDB native by engine time.

| Engine | Query timer sum | Process wall sum | CPU sum | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| DuckDB native | 0.542 s | 1.444 s | 3.868 s | 313.4 MiB |
| rudb native | 0.679 s | 1.213 s | 5.255 s | 202.7 MiB |
| DuckDB Parquet | 1.580 s | 3.208 s | 8.064 s | 355.6 MiB |
| rudb Parquet | 0.926 s | 1.177 s | 5.081 s | 186.9 MiB |

All 43 queries completed. Query 24 output matched the previous plan byte for byte. The deterministic
correctness verifier passed its rewritten checks, including Query 40, and Query 29 matched in the
full audit. Native suite time is still 1.25 times DuckDB, so the 10x target remains open.

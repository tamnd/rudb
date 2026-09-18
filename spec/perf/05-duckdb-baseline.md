# The DuckDB numbers, taken once so they do not have to be taken again

Run on gamingpc-wsl, ClickBench, three runs per query, rudb at cf2fd1e which is the head of main after #518. DuckDB is 1.4.2 out of `/usr/local/bin/duckdb` reading its own format, rudb reading the Parquet.

The point of writing them down is that DuckDB does not change between our pull requests, so an A/B during development can run `--engines rudb` on its own and compare against this table. Re-take it when DuckDB is upgraded, when the machine changes, or when the harness changes what it measures.

| rows | duckdb query time | rudb query time | ratio | duckdb cpu | rudb cpu | duckdb peak RSS | rudb peak RSS |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1k | 126.000ms | 23.418ms | 0.20x | 800.000ms | 0.000us | 52.71 MiB | 6.01 MiB |
| 10k | 155.000ms | 67.173ms | 0.45x | 870.000ms | 0.000us | 53.36 MiB | 8.43 MiB |
| 100k | 322.000ms | 538.249ms | 1.78x | 1.150s | 330.000ms | 81.24 MiB | 34.38 MiB |
| 1m | 556.000ms | 2.020s | 3.91x | 3.460s | 5.870s | 303.77 MiB | 282.36 MiB |
| 10m | 3.063s | 12.999s | 4.82x | 37.670s | 83.350s | 1.60 GiB | 2.23 GiB |

A ratio under one is rudb ahead. The 41 in the ratio row is the shared queries, since q19 and q33 do not run on rudb until the aggregate spills.

## What the shape of that column says

We win small and lose large, and the crossover is between a hundred thousand and a million rows. That is not a surprise and it is not a thing to be pleased about: at 1k and 10k what is being measured is mostly the cost of starting, and DuckDB pays a load step there that we do not because we read the Parquet in place. The load row says 1.983s at 1m for DuckDB and nothing for us, and that is not in the query time column for either of us.

The number that matters is the 10m row and it is 4.82x behind with 2.2x the CPU and 1.4x the memory. Ten times faster and ten times less means that row has to read about 0.3s against 3.06s, so there is a factor of forty between where it is and where it is going.

CPU is the more interesting of the two. We burn 83.3 CPU seconds to DuckDB's 37.7 and we are on the same thirty two threads, so a little under half of the gap is that we are doing more work per row rather than less of it at once. Parallelism cannot close that half. The storage format and the kernels have to.

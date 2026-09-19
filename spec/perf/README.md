# The road to ten times

Notes written on 14 September 2026, after auditing tamnd/rudb-bench#71 and re-running the ClickBench ladder from 1k to 1m rows on `gamingpc-wsl` against rudb at commit `cfdf975`, which is 0.3.6 plus the three top N changes.

The goal has not changed. rudb is meant to be ten times faster than DuckDB and to use ten times less resource on any benchmark. What these notes do is say what that means as a number, where the number is today, and which of the F milestones actually moves it.

## The four files

1. [`01-audit.md`](01-audit.md) is the audit of the root cause report in rudb-bench#71. Five of its claims hold up against the source, one of its conclusions is size dependent and stated too broadly, and it leaves out the largest line item on the board.
2. [`02-budget.md`](02-budget.md) is the arithmetic. What ten times costs in CPU seconds and in bytes, split per operator and per query, with the reduction each one has to deliver.
3. [`03-roadmap.md`](03-roadmap.md) is the F series re-ordered by measured cost, with the critical path and the first ten pull requests on it.
4. [`04-loop.md`](04-loop.md) is the development loop. Run 1k to 1m on every change because it takes four minutes, and hold anything that claims a number to 10m.
5. [`05-duckdb-baseline.md`](05-duckdb-baseline.md) is what DuckDB costs on the same box, which is the number every target here is a fraction of.
6. [`06-partitioned-aggregation.md`](06-partitioned-aggregation.md) is DuckDB's two phase radix partitioned aggregate hash table, read out of their source and their write up, and what it means for F5.
7. [`07-the-root-causes.md`](07-the-root-causes.md) is the five causes the profile actually names, as opposed to the ones it was assumed to name.
8. [`08-where-the-scan-goes.md`](08-where-the-scan-goes.md) splits the scan's own seconds, which were 57.7 percent of everything and unattributed.
9. [`09-the-measured-work-list.md`](09-the-measured-work-list.md) is the work sorted by measured cost rather than by plan order.
10. [`10-what-the-encoder-costs.md`](10-what-the-encoder-costs.md) is `cargo xtask encode`, the per candidate split of the encoder's seconds, and its thread scaling.
11. [`11-how-duckdb-reads-parquet.md`](11-how-duckdb-reads-parquet.md) is `cargo xtask parquet`, both engines over the same file, the two root causes it separates, and the six item work list that follows.
12. [`12-the-chunk-and-the-page.md`](12-the-chunk-and-the-page.md) is what a chunk costs before it holds any data, why the in memory table cannot hand one out without copying it, and the four changes that follow from those two numbers.

[`../storage-v2/`](../storage-v2/) is the format these notes keep arriving at, designed from the queries rather than from the file, with the size target worked out against the real `hits.parquet` instead of assumed.

## The short version

At one million rows rudb answers the 41 ClickBench queries it can answer in 4.568 seconds of query time against DuckDB's 0.533, which is 9.25 times behind, and it does it in 172 MiB against DuckDB's 309, which is 1.8 times ahead. Ten times means 53 milliseconds and 31 MiB.

We are not losing because of any one slow loop. We are losing because of four things that multiply together, and no single one of them closes the gap:

| what | what it costs today | what it has to become |
| --- | --- | --- |
| One thread where DuckDB uses six | 1.0 core against 6.5 | 16 effective cores or better |
| Parquet decoded inside every query | 52.9 percent of our CPU | a native format read once at load |
| A group key that is a `Vec<Value>` on the heap | 27.6 percent of our CPU and all of our peak memory | a code or a packed integer in an arena |
| A per query floor of 3.2 ms at one million rows | below DuckDB's 12 ms, above the 1.3 ms target | metadata read lazily, no fixed work per column |

The first is F4, the second is F2, the third is F5, and the fourth is F2 again. F1 owns the kernels underneath all of them and is worth about 10 percent on its own.

The order that follows from the measurement is F5 before F4 before F2, because a parallel driver over an aggregate that refuses a second instance buys nothing, and because the compact group key is both the thing F4 needs to merge and the thing the memory target needs. That is a change from the order the milestones were written in, and it is the whole point of these notes.

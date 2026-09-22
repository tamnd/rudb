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
13. [`13-what-tpch-costs-in-instructions.md`](13-what-tpch-costs-in-instructions.md) is the first TPC-H note and the first one measured in instructions retired rather than seconds. It says rudb does 2.2 times the work DuckDB does on SF1, that most of the engine is already past ten times, that all of the gap is in one operator, and that closing it entirely still leaves the goal out of reach, so it separates the work that reaches parity from the one idea that reaches ten times.
14. [`14-what-a-group-costs.md`](14-what-a-group-costs.md) takes the top of note 13's work list apart. It measures what one more aggregate call costs at six groups and at a million and a half, finds that the added cost is mostly per group rather than per row, and splits an added call by instructions into starting a state, finishing one into a value, reading the column and the addition itself. The largest single item is the finalise, which builds a `Value` per group per call and then copies it into the output vector, and that is the one change it moves to the front of note 13's order.

15. [`15-what-a-row-costs.md`](15-what-a-row-costs.md) is the other half of note 14, measured per row rather than per group, and it is the half TPC-H pays for. It shows that scan, filter and a small group table are only 1.33 times behind, that every added aggregate call costs 60 to 75 instructions a row against duckdb's 3 to 35, and that neither the column's cardinality nor its packing changes that. It counts the scatter's loop out of the disassembly at forty five instructions a row, of which seven are the query, and it finds a grouped `min` over a `DATE` costing 776 instructions a row because the type was missing from one list.

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

## The short version on TPC-H, added 22 September 2026

Note 13 is the same exercise on the other benchmark and it lands somewhere different. Measured in instructions retired rather than seconds, rudb does 2.2 times the work DuckDB does on TPC-H SF1. Most of the engine is already past ten times: the process floor is 19 times cheaper, an ungrouped sum beats DuckDB, and `count`, `min` and `max` are answered out of the committed directory without reading the page at all. Every bit of the 2.2 times is in the grouped aggregate, and it is there twice over, once per aggregate call because the state is a tagged enum in a side vector rather than a row in the table, and once per row because the partition copies the input instead of the table.

The number that decides what to do is this. Ten times DuckDB on the suite means 2.39 G of instructions against the 52.98 G rudb spends today, so the engine has to do twenty two times less work. Closing the whole aggregate gap is worth 2.2 of that. The rest has to come from not touching the rows, which means deciding the group at load time rather than at query time, and that is the same thesis [`../engine-v2/13-encoded-execution.md`](../engine-v2/13-encoded-execution.md) argues from the layout side.

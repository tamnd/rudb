# What ten times costs

All numbers from one run on `gamingpc-wsl`, 32 hardware threads, 14 September 2026. rudb at `cfdf975`, which is 0.3.6 with the three top N changes on top. DuckDB v2.0.0-dev84237. ClickBench at one million rows, one row in every hundred of the real file, five hot runs per query in fresh processes. Not publishable under rule seven and not meant to be, since it is the development ladder.

## Where we stand

| | DuckDB | rudb | ratio |
| --- | ---: | ---: | ---: |
| query time, 41 shared queries | 533 ms | 4,568 ms | 8.57 times behind |
| query time, as the harness ratios it | 1.00x | 9.25x | |
| CPU over the hot runs | 3.560 s | 4.280 s | 1.20 times behind |
| cores kept busy | about 6.5 | 1.0 | |
| peak RSS | 308.86 MiB | 172.06 MiB | 1.80 times ahead |
| load before the first query | 1.963 s | none | |
| bytes on disk | 501.01 MiB, own format | 211.28 MiB, Parquet | 2.37 times ahead |

Two targets follow from the goal, and they are targets against the same run on the same machine:

- query time at or under **53.3 ms** over the 41 queries, which is one tenth of DuckDB's 533
- peak RSS at or under **30.9 MiB**, which is one tenth of DuckDB's 308.86

CPU seconds are not one of the two targets but they are the constraint that decides whether either is reachable, because wall clock can be bought with cores and CPU cannot.

## The CPU line

Wall clock is CPU divided by the cores actually kept busy. We are at 4.280 CPU seconds on one core. To land at 53.3 ms:

| effective cores | CPU seconds allowed | reduction needed from 4.280 |
| ---: | ---: | ---: |
| 8 | 0.426 | 10.0 times |
| 16 | 0.853 | 5.0 times |
| 24 | 1.279 | 3.3 times |
| 32 | 1.706 | 2.5 times |

Thirty two effective cores on queries this short is not a thing that happens, and twenty four is optimistic. Sixteen is the number to plan against, which puts the CPU budget at **0.853 seconds** and the reduction at **five times**. That is the single number the whole roadmap serves. Anything that does not either cut CPU or convert CPU into cores is not on the critical path.

## The CPU budget, per operator

What the harness says we spend it on at one million rows, and what each has to become to fit in 0.853 seconds with room for a scheduler:

| operator | today | share | per row | budget | reduction | who owns it |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| FileScan | 2.363 s | 52.9% | 57.6 ns | 0.350 s | 6.8 times | F2, then F7 |
| Aggregate | 1.230 s | 27.6% | 69.8 ns | 0.150 s | 8.2 times | F5 |
| Filter | 0.479 s | 10.7% | 18.4 ns | 0.100 s | 4.8 times | F1, then F7 |
| Fetch | 0.355 s | 8.0% | | 0.050 s | 7.1 times | F1 |
| Project | 0.024 s | 0.5% | 1.1 ns | 0.024 s | none | nobody |
| total | 4.453 s | | | 0.674 s | 6.6 times | |

The budget column adds up to 0.674 seconds, under the 0.853 line, and the gap between them is what the scheduler and the per query floor are allowed to cost.

Three things about that table are worth saying out loud.

FileScan at 52.9 percent is not a decoder that needs tuning. It is a structural comparison problem: DuckDB paid 1.963 seconds once to convert the file into its own format and is then timed on that, and we decode Snappy Parquet inside every query and show an empty load column. The callgrind profile of a single query says roughly forty percent of scan time is Snappy, twenty percent is UTF-8 validation, twenty percent is the hybrid run length decoder and the rest is page assembly and copying. A format with no block compression, validated once at load, and bit packing a kernel can read in place removes most of that rather than speeding it up. Six point eight times is aggressive and it is the right kind of aggressive, because it is asking a different question rather than asking the same question faster.

Aggregate at 8.2 times is exactly what the F5 issue already sets as its own exit criterion, so that number is not new, it is just now attached to a measurement.

Project at 1.1 ns a row is finished. It is in the table only so that nobody spends a week there.

## The memory line

Peak RSS is set by the single worst query and nothing else, so the target is a statement about every group by in the suite rather than an average.

| query | today | what holds the memory |
| --- | ---: | --- |
| q34, group by a long string | 172.06 MiB | about 600,000 distinct URLs, owned as strings |
| q35, the same with a constant key | 172.05 MiB | the same |
| q17, group by two very high card columns | 86.44 MiB | a `Vec<Value>` key per group |
| q29, group by a regular expression | 77.01 MiB | extracted hosts, each a fresh allocation |
| q23, two substring scans and a group by | 62.07 MiB | |
| q1, our floor | 5.47 MiB | DuckDB's floor on the same query is 40.09 MiB |

The floor is already seven times better than DuckDB's and the peak is 1.8 times better, which says the problem is entirely in the shape of the group table.

q34 is the one that decides whether 30.9 MiB is reachable. Six hundred thousand URLs averaging sixty bytes is 36 MB of string bytes before any table structure exists, so no arena, no compaction and no key packing gets that query under 31 MiB while it holds the strings. The only way is to not hold them: group on a dictionary code assigned at load time, in a dictionary that is scoped to the column rather than to a block. That is an F2 item, it is already written into the F2 issue, and it is the load bearing dependency of the memory half of the goal.

Parallelism pushes the other way. Sixteen threads each building their own hash table is sixteen tables, and the naive version of F4 plus F5 makes the memory number worse by an order of magnitude. So the aggregate has to be partitioned by hash rather than replicated per thread, which is the radix partitioned implementation the F5 issue lists, and it has to be the default rather than one of the seam's options.

## The per query floor

Ten times below 533 ms over 41 queries is 1.3 ms a query. Our cheapest query today is `SELECT count(*)` at 3.2 ms, and it reads no column. So the target is under our floor and the floor is part of the work.

Most of those 3.2 ms is opening a 105 column Parquet file and parsing metadata for all of it. F2's lazy column metadata is the item, and until it lands every query in the suite carries a few milliseconds it has no reason to carry.

This also sets a limit on how the scheduler may be built. DuckDB's floor at this size is about 12 ms, which is mostly its pool waking up, and that is a four times lead we currently hold. A scheduler that takes a millisecond to start a query spends the whole lead.

## Where the time actually is, by query

Eight queries are 53 percent of the total:

| query | rudb | DuckDB | behind by | shape |
| --- | ---: | ---: | ---: | --- |
| q24 | 528 ms | 60 ms | 8.8x | select star and top k, so this is the Fetch |
| q23 | 412 ms | 44 ms | 9.4x | two substring scans and a group by |
| q34 | 298 ms | 52 ms | 5.8x | group by a long string |
| q35 | 294 ms | 53 ms | 5.6x | the same with a constant key |
| q29 | 270 ms | 112 ms | 2.4x | group by a regular expression |
| q40 | 254 ms | 29 ms | 8.9x | date range, a case and a wide group by |
| q38 | 209 ms | 25 ms | 8.2x | date range and group by a title |
| q22 | 203 ms | 38 ms | 5.3x | substring scan and group by |

Every one of them is a scan plus a group by, and q24 is a scan plus a fetch. There is no fourth shape in the list.

We already win twelve of the forty one: q1, q2, q3, q4, q7, q8, q11, q12, q20, q30, q42 and q43. Every one of those is a query where DuckDB's fixed cost is most of its number, and every one of them will stop being a win at ten million rows. They are worth keeping and they are not worth quoting.

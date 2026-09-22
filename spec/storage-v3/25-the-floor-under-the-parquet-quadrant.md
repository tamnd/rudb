# The floor under the Parquet quadrant

## Why this document exists

Document 24 ended by saying that the Parquet quadrant's grouping has to be fast over flat strings, and said nothing about how. Before designing that, it is worth knowing what it could possibly be worth, and that turns out to be answerable without designing anything: measure the query with the grouping removed, on both engines, and the difference between that and the whole query is the largest prize any grouping design can win.

This document does that. The answer is that the Parquet quadrant cannot be ten times faster than DuckDB on this query by any grouping design whatsoever, because DuckDB's scan alone is more than four times the whole ten times budget. It also finds that the other half of the target, ten times less resources, is reachable in this quadrant and that rudb is already most of the way there in the part of the query that is not the grouping.

Every figure below is the best of three alternating runs of the two engines on one host, and the two columns are separated because they behave differently: user time on this query is repeatable to under one percent across a load average that swung between 14 and 35, and system time is not.

## The scan floor

Two queries over the same column of the same file, neither of which groups anything. The first counts rows that pass the filter. The second sums the lengths, which forces every surviving string to be materialised rather than merely examined, and is therefore the closer stand in for what a grouping has to be handed.

| | user | system | wall | peak resident |
| --- | ---: | ---: | ---: | ---: |
| rudb, count only | 23.51 s | 5.49 s | 15.82 s | 118 MiB |
| DuckDB, count only | 15.81 s | 3.90 s | 12.24 s | 883 MiB |
| rudb, sum of lengths | 31.77 s | 7.14 s | 18.79 s | 118 MiB |
| DuckDB, sum of lengths | 20.29 s | 4.04 s | 13.89 s | 821 MiB |

rudb decodes this column 1.49 times slower than DuckDB when it only has to look at the strings and 1.57 times slower when it has to build them. That is the format tax document 23 named, measured against the other engine rather than against rudb's own native reader, and it is a real gap that is nowhere near ten times in either direction.

The memory column is the surprise. Reading and filtering 100 million rows of a 115 byte string column costs rudb 118 MiB and costs DuckDB about 850 MiB, which is 7.3 times. Neither engine was given a memory limit. rudb is streaming the column and DuckDB is holding considerably more of it, and on this axis rudb is already within striking distance of the target while doing nothing clever.

## The whole query, both engines

The same measurement for the query document 23 and 24 have been working on, which is the scan above plus a grouping into 19,720,796 groups and a top ten.

| | user | system | wall | peak resident |
| --- | ---: | ---: | ---: | ---: |
| rudb | 93.64 s | 53.39 s | 45.17 s | 3,884 MiB |
| DuckDB | 48.29 s | 18.52 s | 14.91 s | 4,869 MiB |

Subtracting the sum of lengths row from this one gives what each engine's grouping costs on top of reading the column:

| | user | system | peak resident |
| --- | ---: | ---: | ---: |
| rudb, grouping alone | 61.87 s | 46.25 s | +3,766 MiB |
| DuckDB, grouping alone | 28.00 s | 14.48 s | +4,048 MiB |

So rudb is 1.94 times behind on user time over the whole query, 1.57 times behind on the scan, and 2.21 times behind on the grouping. The system time is the hash table's page work and it more than triples when the grouping is switched on, on both engines, which is the 15% document 24's profile attributed to the kernel showing up as a measurable quantity rather than a profile share.

## What the floor says about ten times

Ten times better than DuckDB on this query means 4.83 seconds of user time, against DuckDB's 48.29.

rudb's scan, with no grouping at all, is 31.77 seconds of user time. DuckDB's scan, with no grouping at all, is 20.29. Both of those are already more than four times the entire budget, and they are the cost of getting the bytes off the disk and turning them into strings, which is work the query cannot skip and which neither engine can decline.

That settles something this series has been circling since document 13. **No grouping design can make the Parquet quadrant ten times faster than DuckDB on this query.** A grouping that cost exactly nothing would leave rudb at 31.77 seconds of user time against DuckDB's 48.29, which is 1.52 times, and including system time it would leave rudb at about 38.9 seconds against DuckDB's 66.81, which is 1.72 times. That is the ceiling, it is what perfection buys, and the whole of the remaining gap between here and there is 61.87 seconds of user time and 46.25 of system.

Ten times on time in this quadrant would require rudb to decode Parquet roughly five times faster than DuckDB does, which is a claim about a decoder and not about an aggregate, and nothing measured in this series suggests it. The honest statement is that the target's first half is out of reach here and the reason is arithmetic rather than effort.

## What the floor says about resources

The other half of the target is ten times less resources, and it reads very differently.

rudb reads and filters this column in 118 MiB. It groups it in 3,884 MiB. The grouping is 97% of the memory the query uses, and all of it is one hash table holding 19,720,796 string keys that the query then throws away in order to print ten rows.

DuckDB uses 4,869 MiB for the same query. If rudb's grouping could be bounded to a few hundred megabytes, rudb would be somewhere between fifteen and forty times under DuckDB on this query, and the second half of the target would be met on this quadrant with room to spare. The scan measurement says rudb's machinery can already hold 100 million rows in 118 MiB, so the number to aim at is not speculative.

This inverts what the series has been chasing. The grouping is worth attacking, but for what it stores rather than for what it costs, and the design that follows from that is a different one.

## The shape the suite actually asks for

Twenty of ClickBench's forty three queries are `GROUP BY` something, `ORDER BY` a count descending, `LIMIT` a handful. Four more order by a `COUNT(DISTINCT)`, two by an average, one groups without ordering, and one orders by the key. So just under half the suite asks a question whose answer is ten rows and which is currently answered by materialising every group in the input.

The query this series has been measuring returns ten rows out of 19,720,796 groups. Queries 32 and 33 group by `WatchID, ClientIP`, which is close to unique, and return the ten largest counts out of something near 100 million groups. Query 34 groups by `URL` and returns ten. In every one of them the engine builds a table proportional to the input's cardinality to answer a question whose answer is bounded by a constant in the query text.

That is the fundamentally less work this series has been looking for, and it is not in the storage layout. It is in the aggregate refusing to remember what it cannot return.

## What a bounded top count would have to do

Two distributions, and they want different structures.

Where the counts are skewed, which is the `Referer` and `URL` and `SearchPhrase` case, a counter summary of fixed size finds the heavy values. Keep k counters. A key that is present takes its counter; a key that is not evicts the smallest, taking that counter's value as its floor and recording what it inherited as its error. A key whose error is zero was never evicted and its count is exact. The top ten certify when the tenth candidate's count exceeds the largest count any evicted key could have reached, which is the smallest value in the summary, and that is a condition the operator can test at the end rather than assume. On this query the tenth place holds 247,459 rows out of 81,032,736, so a summary of a million counters, which is tens of megabytes, has a slack of two orders of magnitude.

Where the counts are flat, which is the `WatchID, ClientIP` case, no counter summary works, because the answer is a count of two or three and the error bound of any bounded summary exceeds it. There the question is really which keys occur more than once, and a filter that remembers only whether a key has been seen answers it in bounded space, after which the candidates that survive are few enough to count exactly.

Both of these are bounded in memory by a constant the planner picks and neither is bounded by the input's cardinality. Both need an exact fallback for the case where certification fails, which is not a fast path that might be wrong but a fast path that knows when it does not know.

## What this document does not claim

It does not claim either structure is built, or measure one. It prices the prize and names the shape, and the next document in this series should be the one that builds it and reports what it actually cost.

It does not claim the 1.57 times scan gap is irreducible. It is a measurement of two decoders, and document 24 established that this column arrives mostly as plain bytes, so the gap is in plain byte array decoding and UTF-8 validation rather than in anything dictionary shaped. It is the only route to time in this quadrant and this document has not looked at it.

It does not measure the native quadrant's floor. DuckDB's own format was not loaded on this host and the disk would not hold it, so the four way framing the target asks for still has one corner measured only at suite level.

It does not claim the memory figures are anybody's best. Neither engine was given a limit, DuckDB's spilling behaviour under one is unmeasured, and document 22 records a case in this series where an unlimited peak turned out to be policy rather than need. A bounded rudb would be compared against a bounded DuckDB before any ratio here is published.

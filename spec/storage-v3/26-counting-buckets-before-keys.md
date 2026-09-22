# Counting buckets before keys

## Why this document exists

Document 25 measured the floor under the Parquet quadrant and found the time half of the target unreachable and the resource half wide open: rudb reads this column in 118 MiB and groups it in 3,884 MiB, and the whole of that difference is one table of 19,720,796 string keys built to print ten rows. It named two bounded structures and priced neither.

This document replaces both with one that is simpler than either, proves it exact rather than approximate, and measures the one property it depends on against the real file. It does not build it. What it settles is that the structure is sound, that the file's shape gives it two orders of magnitude of margin, and that it buys the resource half of the target at a cost in time that this document states rather than hides.

## The idea

A count is non-negative, so a bucket's count bounds the count of every key in it. That one sentence is the whole design.

Pass one: take an array of M counters and, for every row that passes the filter, add one to the counter at `hash(key) mod M`. No keys are stored, nothing is compared, nothing is inserted, and the array never grows. For M of sixteen million and a `u32` counter that is 64 MiB, fixed before the first row arrives.

Pass two: let T be the Bth largest counter, for a candidate budget B the planner picks. Count exactly, and only, the keys whose bucket holds at least T. Those are few, and the table that holds them is small.

Certify: let `c_k` be the kth largest exact count that pass two produced. If `c_k` is at least T, the answer is exact and the operator says so.

## Why the certification is a proof and not a hope

Take any key the second pass did not count. It lives in a bucket below T, so that bucket's counter is less than T. That counter is the sum of the counts of every key in the bucket and every one of those counts is at least zero, so the key's own count is at most the counter, which is less than T, which is at most `c_k`. A key with a count below `c_k` cannot enter the top k. So no uncounted key belongs in the answer, and every counted key has an exact count because the second pass counted keys and not buckets.

The argument needs the aggregate to be non-negative and additive. `COUNT(*)` is both. `SUM` of a column that can hold a negative is neither, and the design does not apply to it.

It also needs the certification to be checked and not assumed, and needs somewhere to go when it fails. When `c_k` comes back below T the operator has learned that the top k are closer together than the bucket budget can separate, and it still holds the counter array, so it can raise B and run the second pass again, or give up and run the ordinary aggregate. That is a fast path that knows when it does not know, which is the property document 25 asked for.

## What the file says

Buckets of the `Referer` column of `hits.parquet`, M of 16,777,216, over the 81,032,736 rows that pass the filter:

| | count |
| --- | ---: |
| buckets that hold anything | 11,599,999 |
| 10th largest bucket | 247,462 |
| 1,000th largest bucket | 3,511 |
| 100,000th largest bucket | 51 |
| 10th largest key, which is the answer's last row | 247,459 |

The top ten buckets are the top ten keys, give or take the three rows that collisions added to them. That is the first thing to notice and the second is more useful: the thousandth bucket holds 3,511, and the answer's last row holds 247,459, so certification at a budget of a thousand passes with seventy times more margin than it needs.

A budget of exactly ten would not pass. T would be 247,462 and `c_k` would be 247,459, and the operator would correctly refuse to certify and go round again. The budget has to exceed k by enough to absorb what collisions add, and nothing but the data says how much, which is why the operator tests rather than assumes.

Sizing the second pass, over the top thousand buckets:

| | count |
| --- | ---: |
| rows in them | 23,917,644 |
| distinct keys in them | 2,271 |

So the second pass counts exactly 2,271 keys out of 19,720,796, and the table that holds them is a few hundred kilobytes. The first pass is 64 MiB and does not grow. Against the 3,884 MiB rudb uses on this query today that is sixty times less, and against DuckDB's 4,869 MiB it is seventy six times less.

## What it costs, which is a second pass

There is no free version of this. An exact answer to a top k question needs either a table the size of the input's cardinality or two looks at the data, and this design chose the second.

The first pass is the scan plus a hash and an increment, and document 25 measured the scan at 31.77 seconds of user time when it has to build the strings. The second pass is the scan again. So the floor for the two of them together is 63.5 seconds of user time against DuckDB's 48.29 for the whole query, and this design is slower than DuckDB on time while using a seventy sixth of the memory.

It is still faster than rudb is today, whose 93.64 seconds of user time and 53.39 of system are one pass plus a four gigabyte table. Removing the table removes the 46 seconds of system time document 25 attributed to its page work, and the projected total lands near 75 seconds against today's 147. That is a model and this series has a documented habit of getting models wrong, so it is written here as the thing to check first and not as a result.

The second pass can be made much cheaper by keeping what the first one computed. Eighty one million rows of 64 bit hash is 648 MiB, and a second pass over that array needs no decoding at all: it is a stream of hashes and 23.9 million increments into a table of 2,271 entries, which is seconds rather than half a minute. That version lands near 40 seconds of user time, faster than DuckDB rather than slower, and uses about 712 MiB, which is under DuckDB by 6.8 times rather than 76.

So there is a curve and not a point, and the planner picks a place on it:

| | peak | user time, projected |
| --- | ---: | ---: |
| rudb today | 3,884 MiB | 93.64 s |
| DuckDB today | 4,869 MiB | 48.29 s |
| buckets, decode twice | 64 MiB | about 75 s |
| buckets, keep the hashes | 712 MiB | about 40 s |

Both ends beat rudb today on both axes. The low memory end clears ten times less resources by a factor of seven and misses ten times faster by a factor of six. There is no point on this curve that clears both, which is consistent with what document 25 proved about the floor and is the reason it is drawn here.

**Document 27 built the low memory end and measured it, and the projection above was pessimistic on both axes.** It holds the whole query in 89.3 MiB rather than 64 plus a table, certifies at a threshold of 3,540 against an answer whose last row is 247,459, and counts 3,394 keys exactly out of 19,720,796. Carried into the engine that is about 96 seconds of processor time against today's 147 and about 90 MiB against today's 3,884, which is fifty four times under DuckDB rather than seventy six but against a real measurement rather than an estimate of one.

## What this changes about the target

Twenty of ClickBench's forty three queries are a count grouped by something and ordered by that count, and every one of them today builds a table proportional to the input's cardinality. Two of them, 32 and 33, group by a key close to unique across a hundred million rows, and those are the ones where the table is largest and the answer smallest. This design bounds all of them by a constant the query does not choose.

That makes ten times less resources a reachable claim in the Parquet quadrant, on the shape that is half the suite, from a structure whose correctness is a two line argument. It makes ten times faster no more reachable than document 25 found it, and it does not touch the native quadrant, where the global dictionary already answers this query in 825 MiB.

## What this document does not claim

It does not claim the design is built or that any number in the cost table was measured. The two tables of bucket counts were measured on the real file. Everything in the cost table other than the first two rows is arithmetic over document 25's scan floor.

It does not claim a budget of a thousand is right, or that one file's margin is any other file's. The margin comes from the gap between the answer's last row and the rest of the distribution, and a query whose top k are all close together has no margin and will fail certification and fall back. Queries 32 and 33, whose top counts are a handful of rows out of a hundred million, are the obvious candidates for that and this document has not measured them.

It does not claim the hash is free. Eighty one million strings averaging 115 bytes is 9.3 GB of hashing per pass, and the version that decodes twice pays it twice. Document 24 measured `table::hash` at 2.66% of the current query, so this is a small term, but it is a term the two pass version doubles.

It does not address the aggregates that are not counts. A `MIN(URL)` beside the count still has to be computed for the rows that survive, which is 2,271 groups rather than 19.7 million and is therefore cheaper, but a `COUNT(DISTINCT UserID)` in the `ORDER BY` is not a count in the sense this argument needs and four of the suite's queries have one.

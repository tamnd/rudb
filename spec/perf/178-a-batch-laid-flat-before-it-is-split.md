# 178. A batch laid flat before it is split

## What was slow

A grouping that partitions its input gathers chunks into a batch of about sixteen thousand rows, lays each key, argument and filter column of the batch end to end, and splits the batch once across its partitions. `lay` gave up when a column of any chunk was not flat, and then every chunk was split across all 64 partitions on its own. A number or a date that comes out of a join is often not flat, because it is still packed or still behind the selection its scan kept, so on TPC-H q16 at six threads the aggregate made about six thousand folds of around forty rows each, where one thread made about a hundred folds of two thousand rows. The fixed cost of each fold was most of the extra CPU the query spent at six threads.

## The change

When `concat` cannot lay a number, a date or a boolean column as it is, `lay` flattens each chunk's column and lays the flat columns. Unpacking a few thousand integers costs far less than the folds it saves. A string column is still refused, since flattening one copies its bytes and laying it would copy them again.

## Measured

At SF1 on server2, instructions in millions from perf, two rounds each against main just before this change, six threads. x16 is q16 written by hand as two groupings, with the same answer. The answers to all 22 queries at one and six threads are the same bytes as before.

| query | main | this change |
| --- | --- | --- |
| q16 | 172, 171 | 159, 161 |
| x16 | 365, 369 | 326, 340 |

Counted with a probe on `Aggregate::fold`, x16 went from 6,222 folds to 1,053 at six threads, and q16 makes 392.

At one thread nothing moved, and q03, q09, q10, q13, q15, q18, q20 and q21 did not move at six threads either.

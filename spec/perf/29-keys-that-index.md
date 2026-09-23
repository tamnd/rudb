# Keys that index

Notes written on 23 September 2026, after #1605, while looking at q13, which took 2.88 G instructions and 359 ms of CPU at SF1 against DuckDB's 327 ms.

## The question

q13 joins every customer to their orders with a left join, and the table is built on `o_custkey` over about 1.5 million orders. A profile of it run in a loop put a large share of the join's samples in hashing the driving keys and walking the table to the right slot. The keys of a join like this are integers that sit close together: customer keys run from 1 to 150,000, and so do the part, supplier and order keys of the other queries. For keys like that the key itself says where its rows are, and hashing it only to find the same place again is work that buys nothing.

## What changed

A lookup built on one integer key column now checks the range of its keys first. When the range is at most four places a row, it keeps one head a place instead of a hash table, the place being the key minus the smallest key, and the rows with the same key are chained through the same `next` array the table already used. A probe takes the key, subtracts, checks that the place is inside the range and not empty, and that is the slot. There is no hash, no compare and no second key read.

The build is split the same way the table's is. Rows are dealt into as many partitions as the table would have had, each partition owning a run of places, and each thread fills its own run of heads. A key's rows still come out in the order the side holds them, which the join tests rely on. Null keys are left out, as before.

When the keys are spread too far, when there is more than one key column, or when the column is not an integer, the lookup takes the hash table as it did before. The lookup also counts its distinct keys now, so a side with one row a key is known to be single in both forms.

## Measured

Five runs of each query at SF1, instructions, both built without the size setting #1589 put on the CLI crate.

| query | before | after |
|---|---|---|
| q05 | 1.019 G | 0.920 G |
| q09 | 2.265 G | 2.224 G |
| q10 | 1.784 G | 1.743 G |
| q13 | 2.881 G | 2.602 G |
| q14 | 0.427 G | 0.391 G |
| q18 | 2.335 G | 2.267 G |
| suite | 24.50 G | 23.93 G |

In CPU time over ten runs, q13 went from 359 ms to 298 ms against DuckDB's 327 ms, and q05 from 116 ms to 100 ms against 90. No query got slower, and the answers of all 22 queries are the same bytes as main.

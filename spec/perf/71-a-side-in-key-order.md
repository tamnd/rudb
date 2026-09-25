# 71. A side in key order

## The problem

After #1912 the table q09 builds over `orders` was built on every thread, and it was still 59 ms of a query that ran in 160 to 190 ms at eight threads. On one thread it was 113 ms. Most of that was work the table did not need.

`orders` is 1.5 million rows keyed by `o_orderkey`, a primary key spread over six million values. The join took the direct form: a `head` of four bytes for each of the six million places, a `next` of four bytes a row, and the rows dealt to partitions so that each could write its own share of `head`. That is 24 MB written by the build and read at random by every driving row, to answer a question with a simpler answer. The table is stored in the order of its key, so the rows come out of the scan with their keys ascending and each key once. The rank of a key among the keys is then the row it is in, and the table only has to say which places hold a key.

Two more costs sat around it. The direct form copied the key column into a block of `i64` before reading it, although a laid out `BIGINT` column already is one, and at 12 MB of fresh memory that copy was 11 to 14 ms of page faults. And at eight threads the scan hands its chunks over in the order the threads finish them, so the side was not in key order even though the table was.

## The change

`Lookup` has a third form beside the table and the direct form, for a side whose keys ascend row after row with none twice and no row without a key. The pass the direct form already makes over the keys to find their range now also asks each slice whether its keys ascend, and whether each slice ends below where the next starts. When they all do, the build is a bit for each place, set a slice of rows per thread with one atomic `or` for each word, and a count before each word. Keys with no gap between them do not need even that. There is no `head`, no `next` and no deal. A probe finds a key's slot the way the ranked form always has, and the slot is the gathered row, so `firsts` hands it back without a load and every chain is one row long. For `orders` the bits are 750 KB, which stays in the cache for the whole probe.

The direct form reads a flat `i64` key where it lies and only copies a key of another width or form.

Before the side is laid out, `Probe::in_key_order` reads the first and last key of each chunk. If the chunks are in order already, as they are on one thread, nothing else is read. If sorting them by first key puts them in order with no two overlapping, every chunk is read whole on the lease to prove that its keys ascend with no null, and only then are the chunks moved into that order. Moving them is safe only because the keys are proven distinct, so each key is one row and no chain can come out in another order.

## Results

All 22 answers match main at one and at eight threads. Instructions at SF1 on one thread, on server3:

| query | main | after |
|---|---|---|
| q09 | 1575 M | 1426 M |
| q05 | 707 M | 672 M |
| q03 | 562 M | 545 M |
| q07 | 589 M | 577 M |
| q18 | 837 M | 829 M |
| q12 | 510 M | 511 M |
| q21 | 1263 M | 1271 M |

The `orders` build went from 113 ms to 37 ms on one thread and from 59 ms to 35 ms at eight. q09 at eight threads went from 186 to 231 ms to 109 to 164 ms a run, against 225 ms for DuckDB on the same machine. q21's 0.6 percent is the compare the range pass now makes on every row of the large `lineitem` side it builds, which is not in order.

## What is left

Of the 35 ms the `orders` build takes at eight threads, 32 ms is laying the side out, which is one column per thread, and a packed piece is flattened into a vector of its own before it is copied again into the column. Laying out a side on every thread, with a packed piece decoded straight into its place in the column, is the next change. The `partsupp` build in the same query spends 10 ms in the hash table for 42 thousand rows, which is more than it should.

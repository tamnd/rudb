# 128. The wide map for a key of several columns, and one map up front before the split

## What was slow

TPC-H q16 groups 118,274 rows at SF1 by `p_brand`, `p_type` and `p_size` into 18,314 groups. The three columns come out of the join as codes into stable dictionaries of 25, 150 and 8 values over a size window of 48, so with a null place each the key takes 26 x 151 x 48 places, which is 188,448. The direct map gave a key of several columns at most 16,384 places, so the key was refused and hashed. A row hashed three columns, found its bucket in a table of 18 thousand groups and compared two strings and an integer on every probe, about 360 cycles a row.

The bound was there to keep the map in the second level cache, and for a product most of the places are ones no row lands in. But a place read out of the third level cache is still a tenth of what the hashed probe cost, and the map is cleared once a row group rather than once a chunk.

Raising the bound alone did not reach q16. A table that reads its key by value is held to a budget, so that a key whose window keeps moving does not clear a fresh map for every chunk. The budget is what the table has already read times a rate plus a little slack, and the very first map of a table had read nothing, so a map of 188 thousand places was refused on the first chunk and the table stayed hashed for the rest of the pass.

## The change

A key of one column and a key of several now share one bound of 262,144 places, a megabyte of `u32` per thread.

The table an instance fills before it partitions now has its first wide map for free. The budget gets one more term, a map of the whole bound, for that table only. It is one map per thread per pass. A table that partitions does not get it, because each of 64 partitions clearing its own megabyte up front would be up to 64 MB cleared for nothing.

## Measured

At one thread at SF1 on server2, against main at #2626, eight runs of q16 in one process, four runs of each binary interleaved. The server's load average was around 40. The answers to all 22 queries are the same bytes as before, at one thread and at six.

| binary | instructions (M) | cycles (M) |
| --- | --- | --- |
| main | 1,543 | 1,370 |
| wide map for several columns | 1,333 | 1,193 |

Over one run of each of the other 21 queries the instructions moved by less than one percent, the largest being q02 at 0.8 percent more and q07 at 1 percent fewer.

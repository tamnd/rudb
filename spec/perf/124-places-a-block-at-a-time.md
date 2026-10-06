# 124. Places a block at a time

## What was slow

A coded map's places are worked out by `Coded::places`, which used to clear a vector of one place per row and then make a pass over it for each key column, adding that column's code times its stride. q01 has four key columns, so every place was read and written four times a chunk. The two packed columns, `l_discount` and `l_tax`, were also unpacked 64 codes at a time into a block of `u64` words and then added from there. After note 123 that was the largest single cost in q01's cycles, 14 percent in `Coded::places` and part of the 16 percent in `Packed::unpack`.

## The change

A key with no nulls is now worked out 512 rows at a time. Every key column adds into one block of 512 places on the stack, and the block is then written out once. The places vector is written once a chunk and never read back here.

A packed column adds into the block through the new `Packed::add_codes`, which reads eight codes at a time with the shuffle, shift and mask `lanes::unpack` already uses and adds `(code + lift) * stride` straight into the `u32` places with one multiply and one add of eight lanes. The rows before the first that starts a group of eight, and the last few, go one at a time. A key with a null in it is worked out the old way, a column at a time.

## Measured

Single thread at SF1 on server2, warm instructions and cycles as the difference between eleven runs and three in one process, against main at #2605. The answers to all 22 queries are the same bytes as before at one thread, and q1 and q3 also at six.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q01 | 272 | 241 | 186 | 164 |

q13 and q18 run the same instructions as before.

## What this leaves

`Packed::unpack` is still most of what q01 spends before the adds, now for the two summed columns that `PlaceSums::ready` unpacks into `u64` values. `fold_places` and the pass over the places in `touched` are paid once a chunk, and keeping the place cells across chunks would pay them once a run of chunks instead.

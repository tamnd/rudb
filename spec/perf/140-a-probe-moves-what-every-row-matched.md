# 140. A probe moves what every row matched

## What was slow

A hash join's probe copied its driving chunk before it started, because the answer is written over the same chunk, and then gathered every driving column at the matched rows to build the answer. In TPC-H q09 the join to `supplier` is handed the 319,404 `lineitem` rows of the green parts and every one of them finds exactly one supplier. So the copy was six columns of 319 thousand values and the gather was the same six columns again, read at the positions 0, 1, 2 and on up to the end of the chunk. Together they were about 24 million instructions of the 443 the query took at SF1.

## The change

The probe takes the driving chunk out of its slot rather than copying it, since nothing reads the slot again before the answer goes into it. When the chunk is done and the matched rows are every row once and in order, the driving half of the answer is the chunk's own columns, moved across as they are, and only the build side is gathered beside them. Any other shape of match gathers as before. A mark join takes the chunk the same way and moves its columns in as well.

## Measured

At SF1 on server2, one thread, against main at #2682.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q09 | 443 | 426 |
| q07 | 110 | 107 |
| q10 | 162 | 159 |

q03, q05, q08, q18 and q21 run within two million instructions of before. The answers to all 22 queries are the same bytes as before at one thread and at six.

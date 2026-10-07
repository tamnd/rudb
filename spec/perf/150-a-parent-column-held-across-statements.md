# 150. A parent column held across statements

## What was slow

A link join reads its parent's columns at the parent row of each child row. When the parent does not fit in cache but its projection is narrow, it reads the whole column once per statement and then gathers from it (see note 148). In TPC-H q09 that column is `ps_supplycost` of `partsupp`, 800 thousand rows stored packed. Every run decoded the stored pieces and packed them again into one vector before the first row was gathered, and that came to 39 million of the 366 million instructions a warm run takes at SF1. The page pool already holds decoded pieces of a column a statement reads twice, but a whole parent column was built outside it and dropped at the end of the statement.

## The change

The reader now keeps a slot per column for a whole column, next to the slots it keeps for decoded pieces. A link join that builds a whole parent column puts it there, and the next statement that asks for the same column gets the same vector back without decoding anything. The slot is charged to the page pool at the vector's own footprint and is evicted the way a held piece is, by the clock that drops what was not used since the last sweep. Nothing is held when the pool has no budget, and nothing is held by the last statement of a session, which has no statement after it to save anything for.

A table that is written to gets a new reader, so a held column is never read after the rows it was built from have changed.

## Measured

At SF1 on server2, one thread, against main at #2728.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q09 | 366 | 340 |
| q02 | 23 | 23 |
| q11 | 26 | 26 |
| q16 | 120 | 120 |
| q20 | 106 | 106 |

The saving is less than the 39 million the profile showed, because the profile also counted the zeroing of the allocation the column was packed into, which callgrind inflates. The answers to all 22 queries are the same bytes as before at one thread and at six.

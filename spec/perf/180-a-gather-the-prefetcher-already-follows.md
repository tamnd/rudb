# 180. A gather the prefetcher already follows

## What was slow

A gather of packed codes at rows far apart asks for the cache line of the row sixteen places ahead before it reads each row, so that the misses of a join's rows overlap. Far apart meant only that the rows cover four times as many rows as there are of them. The rows a filter keeps out of a chunk pass that test too, since a quarter of ship dates keeps about one row in twenty seven, but they ascend and lie a few dozen bytes apart, and the hardware prefetcher already streams them in. Each prefetch was a bounds check, a multiply and a divide for the word, and the hint itself, and on q15 that was more than half of what the gather spent. On TPC-H q15 `Packed::values_at` was about a fifth of the query.

## The change

The two packed gathers, `Packed::values_at` and `Packed::codes_into`, and the flat one, `picked`, skip the prefetch when the first row is the lowest, the last is the highest, and the rows they cover average no more than four cache lines apart. Rows from a join come in no order or far apart and still prefetch as before.

## Measured

At SF1 on server2, instructions in millions from perf at one thread, against main just before this change. The answers to all 22 queries are the same bytes as before.

| query | main | this change |
| --- | --- | --- |
| q05 | 100 | 96 |
| q07 | 97 | 93 |
| q09 | 289 | 265 |
| q10 | 135 | 128 |
| q14 | 42 | 39 |
| q15 | 54 | 47 |
| q21 | 254 | 244 |
| all 22 | 2069 | 2003 |

No other query moved by more than two. Cycles on server2 move by about eight percent from run to run, and over four rounds q09 and q21 came out the same as before within that, at one thread and at six, which is the check that the rows a join reaches were not left to wait on their misses.

This was taken back in note 181, which measured it on cycles over nine queries and found it slower at every gap tried.

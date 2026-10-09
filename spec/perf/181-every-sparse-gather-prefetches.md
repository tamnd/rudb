# 181. Every sparse gather prefetches again

## What was wrong

Note 180 let a gather of rows that ascend and lie within four cache lines of each other on average skip the prefetch, on the reasoning that the hardware prefetcher already follows such rows. It saved instructions, 2069 to 2003 million over the 22 queries, and the check on cycles was q09 and q21 only. A profile of q05 afterwards had more than half of `Packed::values_at` stalled on the load right after the skip, and q07 at one thread measured 87 and 88 million cycles before the change against 100 and 100 after it. Rows four lines apart are further than the streamer runs ahead, so it does not hide those misses. At one line apart the skip was still slower than the prefetch.

## The change

The gap check from note 180 is gone. `Packed::values_at`, `Packed::codes_into` and `picked` prefetch every sparse gather again, as they did before it.

## Measured

At SF1 on server2, user cycles in millions, summed over two rounds of the steady state (11 runs less 3, divided by 8). One binary read the gap from the environment, so every column below ran the same code: a gap of 0 is this change, 2048 bits is note 180, and 512 and 1024 were tried between.

| query | T1 this change | T1 note 180 | T6 this change | T6 note 180 |
| --- | --- | --- | --- | --- |
| q03 | 143 | 147 | 160 | 174 |
| q05 | 170 | 198 | 184 | 216 |
| q07 | 179 | 177 | 212 | 240 |
| q09 | 423 | 394 | 456 | 496 |
| q10 | 228 | 261 | 278 | 336 |
| q12 | 128 | 150 | 150 | 171 |
| q14 | 92 | 101 | 110 | 106 |
| q15 | 92 | 114 | 109 | 138 |
| q21 | 346 | 374 | 409 | 382 |
| total | 1801 | 1916 | 2068 | 2259 |

The 512 bit gap came to 1860 and 2153, and the 1024 bit gap to 1927 and 2155, so no gap beat prefetching every row. Server2 moves about eight percent from run to run, which is why the totals are what to read here and not any one query. The lesson is that a change cutting instructions in a gather has to be checked on cycles over the queries it touches, not on two of them.

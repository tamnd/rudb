# 184. A ranged place keeps its count and sums on one line

## What was wrong

An aggregate whose one integer key has a range the planner knows counts into arrays the key indexes, and `group_ranged` kept one array for the row counts and one more per sum. A chunk's rows land at places all over the range, so every add is a miss, and every row of a sum, which the array counts rows for as well, missed once in the counts and once again in the sums. q10 and q15 group that way. On q10 at SF1 `Exchange::count` was about 7 percent of the cycles and on q15 about 8.

## The change

The counts and the sums now share one array of `i64` cells. A place is the count followed by each sum as its low and high half, padded to a power of two cells, so a place never straddles two lines and the sum finds the line the count already brought in. A place is found with a shift. With no sum a place is one cell, and that case counts without the shift. A 128 bit sum is still checked on every add, and adding two instances checks every total. A new test puts two sums beside a count, one of them past 64 bits, and adds two instances together.

## Measured

Instructions and cycles per run at one thread on server2, steady state, main at the same base before and the change after. The machine is shared and its cycles move by 10 to 20 percent, so cycles are each run alternated between the two binaries.

| query | instructions before | after | cycles before | after |
|---|---|---|---|---|
| q10 | 135M | 136M | 105M, 117M | 120M, 116M |
| q15 | 54M | 55M | 35M, 53M | 48M, 56M |
| q13 | 150M | 151M | 95M, 103M | 109M, 89M |
| q18 | 138M | 138M | 74M, 91M | 88M, 81M |
| 24M rows into 1M places, a sum and a count | 947M | 1068M | 3321M, 3281M, 3356M | 3024M, 3296M, 3172M |

At SF1 the places of every TPC-H query fit in the cache, and the change does not show above the noise. Cache misses on q10 went from 1666K and 1721K a run to 1387K and 1428K. The last row is a table built from `range` with a key of a million values, the size of q15's suppliers at SF100, where the places are 32 MB and every add goes to memory. It runs 5 more instructions a row and about 5 percent fewer cycles.

That row also showed what the scattered add really costs once the places outgrow the cache. It was 78 percent of the query at about 108 cycles a row, and it took more than one translation miss a row, since 32 MB of 4 KB pages is far past what the translation buffer holds and on a virtual machine each walk is long. Huge pages on the cells would cover that, but none of the shared servers had any free to hand out, so it is left unmeasured here.

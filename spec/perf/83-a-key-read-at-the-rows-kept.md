# 83. A key read at the rows kept

## The problem

q12 reads five `lineitem` columns and its pushed filter keeps 30,988 of 6,001,215 rows, about half a percent. Four of the columns are the filter's: the three dates, which are packed and compared packed, and `l_shipmode`, which is dictionary codes. The fifth is `l_orderkey`, which the join to `orders` reads and the filter does not.

`l_orderkey` is sorted, so the encoder keeps it as runs whose values are deltas, `RLE(DELTA(...), PACKED)`. A whole read decodes every run and then writes every row out, six million eight byte values, for the filter to keep one row in two hundred. The profile had `memmove` at 14 percent of q12 and the cascade decoder at another 8.

The scan already had a way around this. Its deferred read, `Scan::read_deferring`, leaves the columns the filter does not read for later and reads them at the kept rows only. It deferred string columns always and every other column only when a join's bitmap was measured keeping few rows. A tight pushed filter on its own did not count, so `l_orderkey` was read whole.

There was a second gap under the first. Reading a few rows of an integer cascade page goes through `cascade_at` only when the page has a point form, packed or strided or dictionary codes. A run length page did not qualify, so a read at the kept rows decoded the part whole and gathered from it, which is the cost the deferral was meant to avoid.

## The change

Three things.

1. A run length page is read at the kept rows through `integer::decode_selected`, which walks the run lengths to find the runs the rows fall in and never writes out a row nobody asked for. It still decodes every run length and the deltas behind the run values, so it is not called a point form, and `integer::run_length` names it separately.
2. Once the pushed filter is measured keeping fewer than one row in sixteen, the scan defers every column that a whole read decodes flat, not only the string columns.
3. Which columns decode flat is learned from the first part the scan reads whole, from the form each column comes back in. A packed column, or one that comes back as dictionary codes, costs next to nothing to read whole, and reading it a second time at the kept rows cost q06 and q14 about 7 percent when every column was deferred. Those stay in the first read.

## Results

server3, SF1 native, three copies of each query in one process, `perf stat` against main at `df81f6d9`, per query:

| query | threads | cycles before | after |
|---|---|---|---|
| q12 | 1 | 334 M | 284 M |
| q12 | 8 | 442 M | 284 M |
| q06 | 1 | 127 M | 111 M |
| q14 | 1 | 168 M | 161 M |

Instructions for q12 went from 432 M to 429 M, so the saving is in memory traffic rather than work: `memmove` went from 14 percent of the profile to 2.5. q06 and q14 run the same instructions as before. The answers to all 22 queries are the same as before at one thread and at eight. server3 was shared and loaded while this ran, so the cycle counts move by 10 percent or so from run to run. q12 came out 23 to 27 percent faster in cycles in every run, including the three pairs taken while the change deferred every column.

## What is left

The run lengths and the deltas under the run values are still decoded whole, which is a quarter of the rows each for `l_orderkey`. A run length page could keep a row count every few hundred runs and a delta page a running value at the same places, so a read of a few rows would start from the nearest one.

A scan whose filter does not read a string column never reads a part whole once it starts deferring, so it learns the flat columns only if its first part came through the whole read. A table whose first parts all go through the deferred read defers only its string columns, which is the behaviour before this change.

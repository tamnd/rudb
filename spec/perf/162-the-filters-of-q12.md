# 162. The filters of q12

## What was slow

In a profile of q12 run over and over in one process, two things were nearly a third of it. A sixth was `Packed::against_words`, which compares `l_commitdate` with `l_receiptdate` and `l_shipdate` with `l_commitdate`. Since notes 159 and 161 those dates are held at sixteen bit lanes, but the compare still took each group of eight codes through the shuffle, the shift and the mask that a code of any width needs, two loads and an insert before them, on both sides. Another sixth was `l_shipmode IN ('MAIL', 'SHIP')`, which runs on the rows the date filters left and reads a dictionary code at each of them. Nine tenths of its samples were on that one load. Those rows are a few in a hundred of `lineitem` and the codes are four bytes a row, so each read is a cache line of its own and the core waited out every miss in turn.

## The change

`lanes::against_words` now looks at the two widths first. When both are sixteen, a group of eight codes on each side is one load that widens them to 32 bit lanes, and the add, the compare and the mask of the answer are what they were. Any other pair of widths takes the path it took before.

`select_text` asks for the cache line of the code `PREFETCH_AHEAD` rows ahead in the selection, the way `Packed::values_at` already does for a sparse gather, so the misses overlap. The prefetch is now `rudb_vector::prefetch`, for a slice of anything.

## Measured

At SF1 on server2, one thread, q12 run 21 times in one process, the median of nine such runs, against the build of #2778.

| | main | this change |
| --- | --- | --- |
| user cycles of 21 runs (M) | 2015 | 1725 |
| instructions of a warm run (M) | 90 | 75 |

Measured apart, each against main in a run of its own, the widening load alone took about 8% off the median and the prefetch alone about 5%. q04 and q21 compare the same dates but don't reach this path, and their instructions are unchanged. The answers to all 22 queries are the same bytes as before at one thread and at six.

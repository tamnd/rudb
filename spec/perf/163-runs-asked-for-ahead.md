# 163. Runs asked for ahead

## What was slow

q04 keeps the orders of one quarter and asks of each whether any of its lineitems came in late. The semi join runs as `Siblings`, which hands `Vector::gather_runs` the run of lineitems of each order it keeps, and a packed column unpacks those runs a code at a time in `unpacked_runs`. That loop was 30% of a warm q04, and 88% of its samples were on the one instruction after the first read of a code. The orders kept are about one in 25 and a run is about four rows, so every run starts on a cache line of its own, and the core waited out each miss in turn.

## The change

The loop asks for the cache line of the first code of the run `PREFETCH_AHEAD` runs ahead, the way `Packed::values_at` does for single rows, so the misses overlap.

## Measured

At SF1 on server2, one thread, a query run 21 times in one process, the median of the user cycles of nine such runs (five for q21), against the build of #2785.

| query | main (M cycles) | this change (M cycles) |
| --- | --- | --- |
| q04 | 1512 | 1336 |
| q21 | 4275 | 4230 |

`unpacked_runs` went from 30% of q04 to 13%. q21 reads runs too, but they aren't where its time goes, and the difference there is inside the noise. The answers to all 22 queries are the same bytes as before at one thread and at six.

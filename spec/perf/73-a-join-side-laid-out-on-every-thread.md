# 73. A join side laid out on every thread

## The problem

After #1937 the table q09 builds over `orders` takes a few milliseconds, and the step before it became the whole cost. A join lays its gathered side out as one vector per column before it builds anything, and on `orders` that layout was 32 ms of a 35 ms build at eight threads, and 34 ms on one thread.

Two things made it slow. The layout ran a column a thread, and the `orders` side is two columns, `o_orderkey` and `o_orderdate`, so two threads of eight did all of it. And inside each column the work was done the slow way:

1. `o_orderdate` arrives bit packed, and flattening a packed vector went through the general copy. That copy builds a list of every position and then reads each code on its own, working out its word and whether it straddles into the next each time. For 1.5 million dates that was 10.6 ms on one thread.
2. `o_orderkey` arrives as 184 flat pieces from different pages, so laying them end to end is a copy into a new 12 MB page. That copy was 11 to 14 ms, most of it faulting fresh memory in, and one thread did it while the rest waited.

## The change

A packed vector with no nulls is flattened a block of 64 codes at a time with `Packed::unpack_mapped`, which the scan already uses. The width is a constant in the inner loop, and there is no position list. This is in `Vector::flatten`, so every flatten of a packed column gets it, not only a join.

The layout decodes first. Every piece that is not flat is flattened as its own task on the lease, so the decode takes every thread whatever the number of columns.

A side of 65,536 rows or more then lays its columns one after another, and each column's copy is spread over the lease through `concat_on`. That has a new case for fixed width pieces: the page is allocated zeroed, which for a page this size is fresh memory from the kernel and costs no pass, it is cut into a slice per piece, and each piece is copied into its slice by its own task. The page faults are spread across the threads with the copy. Pieces that are windows of one page still lay as a handle, and a short side is still laid a column a thread.

## Results

All 22 answers match main at one and at eight threads. Instructions at SF1 on one thread, on server3, main at 7c9c07bf against this change:

| query | main | after |
|---|---|---|
| q09 | 1360 M | 1265 M |
| q05 | 642 M | 634 M |

No other query moves by more than 0.4 percent.

The `orders` layout on one thread went from 34 ms to about 17 ms, with the decode from 10.6 ms to 1.5 ms. At eight threads it went from 11 to 13 ms to 5 to 11 ms on a loaded box.

q09 at eight threads, twelve runs each on a quiet server3:

| | best | median |
|---|---|---|
| main | 100 to 106 ms | 124 to 129 ms |
| after | 97 to 100 ms | 104 ms |
| DuckDB | 147 ms | 194 ms |

## What is left

The copy is still a copy. The key column of `orders` is already in memory in the pieces the scan handed over, and a side whose keys are in order (note 71) only ever reads it by row. Reading the pieces where they lie, through a map from row to piece, would take the 12 MB copy out entirely rather than spreading it.

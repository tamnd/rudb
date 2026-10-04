# 109. Closed runs added up from codes, under a HAVING

## The problem

TPC-H q18 groups `lineitem` by `l_orderkey` and keeps the orders whose `sum(l_quantity)` is over 300. At SF1 that is 1.5 million groups, of which 57 pass. The table is sorted by `l_orderkey`, so the aggregate closes each run of the key inside a chunk on the spot, with `Aggregate::close_runs`, and only the first and last run of a chunk go through the table.

Warm, on server2 at one thread, the query was 251 M instructions and 256 M user cycles a run, plus 125 M cycles in the kernel and about 8,200 page faults. Three things went into that.

1. Since note 107, `l_quantity` is held packed. `close_runs` reads integers flat, so it flattened the argument into a fresh vector on every chunk before adding anything up. EXPLAIN ANALYZE counted that as 733 fallbacks, one a chunk.
2. Each run was added up in a loop of its own, and a run of `l_orderkey` is four rows, so the loop was entered and left every four values.
3. Every closed group was answered: the key gathered, the total made into a `DECIMAL(38,2)` answer, and the chunk held until the scan was done. That is about 36 MB at SF1, written into fresh pages on every run, which is where the faults and the kernel time came from. Then the filter above threw away all but 57 rows of it.

## The change

A packed argument with no null is added up from its codes. `packed_run_totals` unpacks the codes of the chunk once into a buffer the thread keeps and turns them into a running total in place, so a run's total is the running total at its end less the one at its start, plus the base once for each of its rows. The running total is of codes, which are at most `width` bits, and the function refuses when the rows times the largest code might not fit in 64 bits.

A `HAVING` that compares one call with a constant from above, `sum(x) > c` or `>= c`, is handed to the aggregate as `having_total`, the least raw answer a group needs. `close_runs` works out that call first, keeps the runs that reach it, and only then gathers keys and makes answers, for those runs alone. This is the same shape as `having_count` for `count(*) >= n`. The filter stays in the pipeline and checks every row again, and the groups that go through the table are all answered as before, so a shape this misses is slower and never wrong. The constant has to be in the terms the answer is made of: an integer for an integer answer and a decimal at the answer's scale for a decimal one.

## Results

Measured on server2 at SF1 on one thread, per warm run, against main with #2441. All 22 queries give the same answers as before.

| Build | User cycles (M) | Kernel cycles (M) | Faults | Instructions (M) | Wall (ms) |
| --- | --- | --- | --- | --- | --- |
| main | 252, 260 | 124, 127 | 8,232, 8,721 | 251 | 135, 130 |
| codes only | 198, 218 | 115, 132 | 8,168, 8,350 | 199 | 121, 147 |
| codes and HAVING | 145, 152 | 7, 8 | 39, 80 | 175 | 51, 54 |

Adding the runs up from codes takes a fifth of the instructions off. Answering only the groups that pass takes nearly all of the kernel time and the faults away, and the query runs in 40% of the time it did.

## What this leaves

What is left of q18 is mostly finding the runs: `interior` checks the key is in order in one pass and `run_starts` compares the same neighbours again in another. One pass could give both.

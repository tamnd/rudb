# 151. Only the build columns read above

## What was slow

A hash join hands up every column of the side it built, next to each driving row it matched. Most of those columns are only there because the build side was a scan or a join that needed them itself, for its own filter or its own keys. In TPC-H q09 the join to `supplier` builds over `supplier` joined to `nation`, which carries `s_suppkey`, `s_nationkey`, `n_nationkey` and `n_name`, and gathers all four for each of the 319 thousand line items that reach it. Only `n_name` is read above. The other three were copied once a row and dropped two operators later by the projection.

The planner prunes the columns of scans and projections, but a join's output is the two sides laid end to end, and nothing narrowed it.

## The change

The executor now asks, for an inner hash join, which columns of the built side anything above it reads. It walks every node the plan runs, outside the built side and other than the join itself, and collects the columns of the built side they name. The join's own conditions do not count, since the probe reads its keys and any residual from the table it built rather than from the columns it hands up. The answer is only trusted when the join's output is narrowed by a projection or an aggregate before it reaches anything that reads its columns by position, which is the same check join elimination makes.

The probe then gathers only the columns on that list and puts a constant null in the place of each of the others, so the column numbers above still line up. When the list is empty, which is what the existing pass through for an exactly reduced driving scan asked before, that pass through is still offered, and if it does not apply at run time no build column is gathered either.

## Measured

At SF1 on server2, one thread, against main at #2728.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q09 | 366 | 358 |
| q07 | 103 | 100 |
| q05 | 101 | 99 |
| q03 | 76 | 75 |
| q08 | 67 | 66 |
| q10 | 150 | 149 |

The answers to all 22 queries are the same bytes as before at one thread and at six.

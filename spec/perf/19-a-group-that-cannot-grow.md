# A group that cannot grow

Notes written on 23 September 2026, after #1406 made the native writer's order summaries correct for tables loaded in parallel.

## The question

TPC-H q18 was the most expensive query on the suite at 6.0 G instructions against DuckDB's 2.0 G. Most of that was one aggregate, `GROUP BY l_orderkey` with a `SUM(l_quantity)` over six million rows and a million and a half groups, which alone was 3.7 G against DuckDB's 1.8 G for the same inner query.

Note 18 made that aggregate cheaper by taking less room in advance. This note asks a different question: why does it need a hash table at all?

## What the table is for

A hash aggregate keeps every group until the last row has arrived, because the next row could belong to any of them. That is the whole reason for the table, the probe per run, the partitioning once a table passes 4,096 groups, and the merge between threads at the end.

lineitem is stored in `l_orderkey` order. Over rows sorted on the key, a group whose key the rows have moved past can never see another row. It is finished the moment the key changes, and a finished group needs no bucket, no hash, no probe and no place in the merge.

## What a chunk can close

One instance does not see the whole table. It sees morsels, and a morsel's rows arrive as chunks, each a contiguous range of row ids with some rows filtered out. The first run of keys in a chunk may have started in the chunk before, which some other instance may hold, and the last run may carry on into the next. Every run strictly between those two is a whole group. If the table is sorted and the chunk is sorted, every row of a key strictly inside the chunk's first and last keys has to sit inside the chunk.

So each chunk is split three ways:

1. Rows before the end of the first run go through the ordinary fold into the instance's table.
2. Rows from the start of the last run go the same way.
3. Everything between is folded into a second table that is never probed. Each run appends one group with no hash and no bucket, then shares the ordinary state update with the rest of the fold.

The closed table becomes chunks when the instance combines, and those chunks wait beside the answer while the partitions finish. The ordinary table ends up holding about two groups per chunk, so it never reaches the partitioning threshold on q18.

## Where the promise comes from

It comes in two halves.

The store's half is the column summary. The binder asks the catalog which columns a native file says never go down and hold no null, and records them on the plan. Only a file answers this. A table in memory, or a file with rows grown past it, answers nothing.

The plan's half is the path between the scan and the aggregate. The `aggregate_cluster` pass walks down from an aggregate with one column key and accepts only filters and projections that carry the column through unchanged. A join, a sort or a union stops the walk.

Neither half is trusted with the answer. Before closing anything, the executor checks that the chunk's key is in ascending order and in a form where two rows compare by value, meaning packed integers or a flat integer array with no null. A chunk that fails the check goes through the ordinary fold whole. A stale summary costs time and never gives a wrong answer.

## What is left out

The exchanges, the dense count, `DISTINCT` calls and a pushed down limit each finish from state of their own, so an aggregate that uses any of them does not close groups. A key made of two columns where the first is the sorted one could close the same way, and is not done here.

It is behind its own setting, `SET stats_closed_groups = 'off'`, and the pass answers to `SET disabled_optimizers = 'aggregate_cluster'`. `EXPLAIN` prints `groups closed in key order` on an aggregate the pass marked.

## Numbers

TPC-H SF1 from a native file loaded by `CREATE TABLE AS SELECT * FROM read_parquet(...)` on a binary with #1406, so the summaries are there. Instructions are the best of three fresh processes on server3. Wall time and peak resident memory are the best of seven on server2, which was idle.

| query | before | after | DuckDB |
|---|---|---|---|
| q18 instructions | 6.012 G | 4.102 G | 1.986 G |
| q18 inner aggregate instructions | 3.681 G | 1.751 G | 1.808 G |
| q18 wall | 0.36 s | 0.23 s | 0.18 s |
| q18 inner aggregate wall | 0.24 s | 0.11 s | 0.14 s |
| q18 inner aggregate peak memory | 200 MB | 189 MB | 128 MB |

The suite total went from 49.591 G to 47.761 G instructions. q18 accounts for all of that, and the other twenty one queries moved by less than one percent either way.

The inner aggregate now uses fewer instructions than DuckDB's and less wall time. The rest of q18 is the joins around it. Peak memory barely moved because the aggregate was not what held it, and that is a separate question about the scan.

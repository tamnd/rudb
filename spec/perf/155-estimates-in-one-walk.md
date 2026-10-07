# 155. Estimates in one walk

## What was slow

Once a query is planned, `record_estimates` writes the estimated row count onto every node, so that EXPLAIN and the later passes can read it. It asked `rows_stat` about each node in turn, and `rows_stat` answers by walking the whole subtree under the node. A node deep in the plan was walked once for itself and once more for every node above it, so the work grew with the square of the plan's depth. TPC-H q02 joins five tables with a correlated subquery over four more, and at SF1 this was 2.3 million of its 23.4 million instructions, a tenth of the query.

## The change

`rows_stats` answers for every node of a plan at once. While it runs, a per thread table holds the answer for each node the first time it is found, and `rows_stat` reads that table before it walks, so each subtree is walked once. The table is dropped when `rows_stats` returns, even on a panic, so nothing from one plan is read for another. That is safe because the plan is borrowed for the whole call and the answer depends only on the plan and the facts. `record_estimates` now takes the answers from `rows_stats`. A test checks that the answers for every node at once are the answers asked one at a time, and that nothing is held after.

## Measured

At SF1 on server2, one thread, against main at #2749.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q02 | 23 | 21 |
| q11 | 26 | 26 |
| q07 | 100 | 100 |
| q09 | 299 | 299 |
| q21 | 246 | 246 |

The other plans are shallow enough that the walk was already small next to the run. The answers to all 22 queries are the same bytes as before at one thread and at six.

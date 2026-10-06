# 143. The producer of a table index without a walk

## What was slow

The optimizer's estimates find the node that makes a table index through `producer`, which follows a column back to the scan it came from to read how many distinct values it has. A pass that rebuilds a node leaves the old one in the arena with the same index, so `producer` walks the plan from the root to find the copy the root still reaches. It did that walk for every call, and the estimates make a call for every column they follow, so planning a query paid for a walk over the whole arena many times over. In the profile of planning q02 the function and the node accessors it calls were about a tenth of the time.

## The change

`producer` first looks for the nodes with the index. When there is only one, that node is the answer whether the root reaches it or not, which is what the walk would have said too, so it is returned without the walk. The walk only runs for an index that a pass has rebuilt and that has more than one node.

## Measured

At SF1 on server2, one thread, against main at #2688. Planning is measured as the difference between running a query's `EXPLAIN` a few times and many times.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| planning q02 | 7 | 6 |
| planning q16 | 3 | 2 |

q02, q05, q07, q08, q09, q11 and q21 run within one million instructions of before. The answers to all 22 queries are the same bytes as before at one thread and at six.

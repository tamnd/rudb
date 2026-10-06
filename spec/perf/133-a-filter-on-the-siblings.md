# 133. A filter on the child is a test for the siblings walk

## What was slow

TPC-H q04 counts the orders of one quarter that have a line received after it was committed. The plan is `orders` semi joined to the lines with `l_commitdate < l_receiptdate`, and the semi join was answered by the hash join turned around: the 57 thousand orders of the quarter were gathered and put in a table, `lineitem` was read through the reduction those orders made, and each line that passed the filter was looked up in the table to mark its order.

The scan of `lineitem` was 63 percent of the query on one thread. Building the reduction from the orders was 8 percent, and a quarter of the query went to giving each kept line its `l_orderkey` back: the link took each line to its order and the key map took the order to its key, only for the hash table to find the same order again.

## The change

The siblings walk of `crates/rudb-exec/src/siblings.rs` already answers this shape from the other end. Each order row goes through the key map to its row, the link gives the run of its lines, the dates are read at those lines and tested, and the order is kept when one passes. Nothing is gathered and `lineitem` is never scanned. TPC-H q21 and q22 go this way.

It refused q04 because the walk is only taken when there is more to test than the one equality, and it looked for that only in the join's own condition. A join of equalities alone is a link join and costs less, which is why the rule is there. But q04's test is a filter over the child, and the walk already reads the filters under the join as tests. Now a filter over the child counts as something to test too.

## Measured

At SF1 on server2, against main at #2642. The load average was between 16 and 20. The answers to all 22 queries are the same bytes as before, at one thread and at six.

Eight steady runs of q04 at one thread:

| binary | instructions (M) | cycles (M) |
| --- | --- | --- |
| main | 139 | 98 to 113 |
| this change | 92 | 67 to 70 |

At six threads, thirty runs took 39 to 49 ms a query on main and 25 to 30 ms with the change.

What is left of q04 is mostly the walk itself: gathering the two dates at the lines is 16 percent, and finding each order's run in the link is 14 percent.

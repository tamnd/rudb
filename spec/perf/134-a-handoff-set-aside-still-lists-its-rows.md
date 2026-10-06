# 134. A handoff set aside still lists its rows

## What was slow

TPC-H q02 asks for the cheapest European supplier of each brass part of size 15. The subquery that finds the cheapest cost is decorrelated into an aggregate over `partsupp`, `supplier`, `nation` and `region`, under a semi join on the 747 parts the outer query is about. Both joins hand their keys down to the one scan of `partsupp`.

A scan reads one handoff as its own and the rest as set aside, see `crate::sideways`. It owns the nearest join, which is the one to `supplier`, and the 1,987 suppliers of Europe are a bitmap there because they reach a fifth of the table and a push through the link could skip nothing. The semi join on the parts was set aside, and a handoff set aside makes no rows, so it was a second bitmap. The scan read all 800 thousand rows of `ps_partkey` and `ps_suppkey` and tested both. The 747 parts are 2,988 rows of `partsupp`, and the outer query's own scan of the same table reads just those through the link.

## The change

The scan already knows how to choose among several sets of keys. A relation of a consistent reduction hands it kept keys as a listing, and the scan asks each listing how many rows it would reach and reads the fewest as rows, testing the rest. A handoff set aside now carries the same listing over its key bitmap, so the scan can read it as rows when it reaches fewer than what its own join left it.

A listing was read through the backward adjacency only, and `partsupp` has none. It is stored in part order, so its link is monotone, and the file keeps no adjacency for a link whose runs it already holds. A listing over such a link is now pushed through it, which walks from one held parent to the next and costs about what the set holds, the same push a join over the link already makes.

With both, the subquery's scan reads the 2,988 rows the parts reach and tests only the supplier bitmap on them.

## Measured

At SF1 on server2, against main at #2646. The load average was between 30 and 40 for these runs, which moves instruction counts by a few million from run to run, so the two binaries were run side by side.

| q02, one thread | instructions (M) |
| --- | --- |
| main | 34 to 37 |
| this change | 28 |

In the profile, testing the subquery scan's rows went from 14.6 percent of the query to 2 percent, and reading every scan from 33 percent to 26 percent. The answers to all 22 queries are the same bytes as before at one thread and at six, and q05, q07, q08, q09, q10, q11, q16, q20 and q21 run the same number of instructions.

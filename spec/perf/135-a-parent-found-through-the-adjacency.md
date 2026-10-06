# 135. A parent found through the adjacency, and a prefix decided on the views

## What was slow

TPC-H q20 asks for the suppliers in Canada with more than half their shipped quantity of a forest part still in stock. The subquery sums `l_quantity` over the lines shipped in 1994 for each pair of part and supplier, under a semi join on the 8,508 pairs of `partsupp` whose part name starts with `forest`. Those pairs hold 2,127 parts.

The semi join hands its keys to the scan of `lineitem`, and a handoff makes exact rows when the build side's keys go through a parent's key map and then through the child's link or adjacency, see `build::exact`. The build side is `partsupp`, whose key is two columns, so it has no key map over `ps_partkey` and the parent has to be found from the child instead, through the link `l_partkey` was built with. That link was over the budget at SF1 and is not in the file, so no parent was found and the handoff was a bitmap over the part keys. The scan read every row of the year's dates and tested the bitmap on what the date filter kept. The backward adjacency from `l_partkey` to `part` was in the file the whole time, and a plain join of `lineitem` to the same 2,127 parts reads the 63,832 rows they reach through it.

The other fifth of the query was the two scans of `part` asking `p_name LIKE 'forest%'` of 200 thousand names each, one for the outer query and one for the subquery. A prefix was decided with a call to compare bytes for every row.

## The change

The parent of a child column is now found through its adjacency when the column keeps no forward link, since the adjacency was built against the same parent and names it at the front of its payload the same way. `listed` reads the adjacency and the parent's key map and never the link, so it turns the 2,127 parts into the 63,832 rows of `lineitem` they reach, and the scan reads those and tests the dates on them.

A prefix `LIKE` over a column of views is decided on the length and the first four bytes, which every view holds whether its string is inline or in the arena. Only a row whose first four bytes match reads the rest from the arena. For `forest%` that is about one name in ninety.

## Measured

At SF1 on server2, against main at #2650, side by side under a load average around 20.

| q20 | instructions (M) | ms, one thread | ms, six threads |
| --- | --- | --- | --- |
| main | 134 to 147 | 40 | 43 |
| this change | 109 to 111 | 30 | 31 |
| DuckDB | 578 | 104 | 189 |

The answers to all 22 queries are the same bytes as before at one thread and at six. q02, q05, q07, q08, q09, q13, q14, q16, q17, q18, q19 and q21 run the same number of instructions.

# 148. A link join over a filtered parent

## What was slow

The link join was only taken when its parent was a bare scan. A filter between the parent's scan and the join, such as `o_orderstatus = 'F'` on `orders` in TPC-H q21, sent the join back to a hash join. That join scanned the parent, kept the rows that passed, and built a table over them, even when far fewer child rows arrived than parents passed. On the count of late Saudi lines joined to orders with status `'F'`, the hash join scanned 1.5 million orders to build a table of about 730,000 of them for 156,739 lines.

A second rule kept the link join out of the top of q21. The child's row id was treated as lost once the child became the build side of another join, because the build side is gathered rather than streamed. The lines of `l1` in q21 are the build side of the anti join and the semi join, so the join to `orders` above them could not read the link either.

## The change

A filter over the parent is a test on the parent row, so the link join now runs it at the parent of each child row it finds. It gathers only the columns the filter reads, and a child whose parent fails the test gets the no parent sentinel. Each kind then does what it already does for a child with no parent, and that is what each kind means over a filtered parent. Inner and semi drop the row, anti keeps it, and left pads it with nulls.

A filtered parent is also the case where a hash join can cut the child down, because the small table of parents that passed sends a reduction to the child's scan. So the planner reads the link over a filtered parent only when the child is estimated at fewer rows than the filtered parent. On q17 the other way round, 204 parts against six million lines, the link join tested the part of every line and cost 30 times the plan it replaced, and the rule now keeps the hash join there.

The row id is a column, and a build side gathers its columns with its rows, so a child on the build side of another join still names its row of the table in every row it reaches. The planner now accepts that, and it still declines a child on the side an outer join pads, or on the side a semi, anti or mark join drops.

## Measured

At SF1 on server2, one thread, against main at #2708.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| `count(*)` of late Saudi lines joined to orders with status `'F'` | 185 | 129 |
| q08 | 80 | 67 |
| q21 | 265 | 258 |

The other queries run as before. On q21 the hash join to `orders` was already cheap, because the reduction from the semi join cut the orders scan to the 8,357 orders it needed, so most of what is left there is the anti join's walk. The answers to all 22 queries are the same bytes as before at one thread and at six.

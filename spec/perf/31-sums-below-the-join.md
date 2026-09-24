# Sums below the join

Notes written on 24 September 2026, after #1610, on the grouping in TPC-H q10 and why it does more work than the answer needs.

## The question

q10 joins customer, orders, lineitem and nation, keeps the returned items of one quarter, and groups on seven columns to sum the revenue of each customer. Four of those columns are strings: the name, the address, the phone and the comment off customer, and the name of the nation. At SF1 the joins hand the grouping 114,705 rows and it ends with 37,967 groups, one per customer. So it hashes and compares four strings for every row, three times as many rows as there are answers.

Every one of those rows is a lineitem joined to its order, and the only thing the groups need from that side is which customer placed the order. Summing by `o_custkey` below the join to customer gives the same 37,967 rows before a single string is read. The join to customer then probes 38k rows instead of 115k, and the grouping over seven columns sees each customer once. DuckDB does not do this. Written out by hand as a subquery, the same answer came in 1.27 G instructions against 1.45 G, which was the reason to make it a pass.

## When it is the same answer

An aggregate over inner joins and filters, with every argument read from one input B below them. Take K to be the columns of B that anything above it reads, which is the group keys and the join conditions and filter predicates on the way down. Two rows of B with the same K meet the same rows on every join above, pass the same filters and land in the same group, so summing them first and then summing the sums gives the sum. A match that repeats a partial row repeats it exactly as it would have repeated the rows it stands for, so duplicates on the other side need no special case. `min` and `max` work the same way, and a partial sum that came out null because every value was null is skipped above the way the values were skipped below.

`count` would have to become a sum of counts, which changes the call and the type, and `avg` would have to be split in two, so neither is moved yet. A `sum` over a float is refused, because adding in another order is another answer there. `DISTINCT` and `FILTER` are refused because neither survives being done twice.

## When it pays

The first version moved the sum wherever the rules above allowed it, and q05 went from 0.92 G to 2.52 G instructions. There the join above B is the one to supplier, which throws most rows away, and the partial sum read every row that join was about to drop. It also sat between the join and the lineitem scan, so the runtime filter that join sends down no longer reached the scan.

So the pass now asks three things of the join directly above B. Its other input has to be a plain scan with nothing filtered, so the join brings columns in and does not drop rows. That input has to supply a string the grouping reads, since a string is what makes each grouped row expensive. And K has to be all fixed width columns, so the partial grouping stays cheap. The estimates cannot tell how many distinct values K has with any confidence, so the pass does not weigh row counts. It tries the highest B first and moves down one join at a time. On q10 it first tries the side under nation, whose K would hold customer's strings, and then the side under customer, whose K is `o_custkey` alone.

## What it did

On SF1, in instructions retired, averaged over five runs in one process:

| query | before | after | DuckDB |
|---|---|---|---|
| q10 | 1.449 G | 1.263 G | 1.454 G |
| suite | 23.56 G | 23.36 G | 25.54 G |

No other query moved and all 22 answers are the same bytes as before. The machine had a load average near 77 while this was measured, so CPU times from that run swung by half in both directions and are not reported here.

## What is left

q18 also groups on customer's name over a sum of lineitem, and the pass leaves it alone as it stands. Why it does, and whether it should not, is the next thing to look at. After that comes moving `count` as a sum of counts, which the counting queries outside TPC-H would use.

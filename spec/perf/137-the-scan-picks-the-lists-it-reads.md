# 137. The scan picks the lists it reads

## What was slow

A join whose build side holds keys of a table's own column, with a backward adjacency to the scan's table, read the children of those keys off the adjacency as soon as its side finished, see `listed` in `sideways.rs`. The scan was then handed those rows whatever else it was handed. When another join reached fewer rows, the scan read those and tested the rest of the joins row by row on them, so the lists it was handed bought nothing and cost the push.

TPC-H q07 at SF1 is the case. The 798 suppliers in France and Germany reach 478,523 rows of `lineitem` through lists in no order, and pushing them is about 44 instructions a row, 17% of the query. The orders of the two years whose customers are in one of the two countries reach 173,383 rows in runs of the link. Once the supplier rows were pushed, the orders' kept keys were refused, because they reach more than one row in sixty four of the table. So the scan read the 478 thousand rows and tested the orders' bitmap on them.

## The change

The lists are counted when the side finishes and read only when the scan asks for them. The join keeps its bitmap over the key values, so a scan that reads another set tests these rows by the bitmap instead.

The scan asks every join how many rows it reaches, takes the fewest that sit sparse enough to read one at a time, and asks again the joins that were refused, this time against that count. A set of kept keys that narrows rows already read one at a time is worth listing at any share of the table. The scan reads the set that reaches fewest. Another set is read alongside it only when the rows in both, taking the two as independent, are expected to touch under half the parts the first touches. A row the second set would remove costs one column read and a bit test after #2660, which is less than listing it, so only parts left unopened pay for a second push. In JOB 24a the rows of `cast_info` in both sets are expected to be about 147 against the 800 parts the first set touches, so both are still read. On q07 the rows in both are expected to be 13.8 thousand against about 2,930 parts, so the supplier lists are never read.

A join whose lists are left for the scan keeps no sorted list of its keys either. The lists or the bitmap settle every row, and the sorted list only had the scan ask the range of every part about it, which put q17 up by two million instructions.

## Measured

At SF1 on server2, one thread, against main at #2660, under a load average around 20.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q07 | 126 | 112 |

q02, q03, q05, q08, q09, q10, q17, q20 and q21 run the same number of instructions. The answers to all 22 queries are the same bytes as before at one thread and at six.

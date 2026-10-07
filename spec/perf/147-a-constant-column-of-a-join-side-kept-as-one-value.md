# 147. A constant column of a join side kept as one value

## What was slow

A join to a side of one row, such as `nation` cut to `'SAUDI ARABIA'`, hands up that row's columns as constants, because every row it produces has the same `n_nationkey` and `n_name`. When those rows then became the side a hash join builds on, laying the side out flattened every column, and a constant has no flat form to copy from, so it went through the general copy that writes one value at a time. On the join of late lines to `orders` in TPC-H q21, that wrote the nation's key and the string `'SAUDI ARABIA'` once for each of 156,739 rows, and the generic push of a value was the biggest single item in the profile.

## The change

When a column of the side is the same constant in every piece, the build keeps it as one constant vector of the side's length and does not decode it. A probe gathers a constant as a constant, so it stays one all the way up the plan. The keys the table is built over are still laid out a row each, so a constant key is flattened before the table reads it.

## Measured

At SF1 on server2, one thread, against main at #2704.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| `count(*)` of late Saudi lines joined to orders with status `'F'` | 258 | 184 |
| q03 | 82 | 76 |
| q05 | 106 | 101 |
| q08 | 85 | 80 |
| q10 | 156 | 150 |
| q21 | 269 | 265 |

The other queries run as before. The answers to all 22 queries are the same bytes as before at one thread and at six.

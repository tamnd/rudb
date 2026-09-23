# Filters through joins

Notes written on 23 September 2026, after #1528, on why q05 spent more than twice DuckDB's CPU when both run the same join order.

## The question

q05 joins lineitem to orders, then the result to customer, supplier, nation and region. Each join builds a runtime filter from its build side and hands it to the scan under its driving side. The join to orders drives from the lineitem scan directly, so its filter reached lineitem. The join to supplier drives from the join to orders, and the handoff only walked through projections and filters, so it stopped at the first join and the filter was never used. The lineitem scan let through 976 thousand rows where 203 thousand can match, and every one of them was probed into the orders table before the supplier join dropped it.

## Passing through a join

An inner join keeps no row its driving side did not have, and every row it keeps carries the driving row's columns unchanged. So a filter on one of those columns is just as true of the rows under the join as of the rows above it. The same holds for a semi join, which keeps a subset of its driving rows. It does not hold for a left, right, full or anti join, because those keep or make rows that no filter above can speak for, and for a mark join, whose output depends on every row.

`sideways::through` names the driving input of an inner or semi join, whichever side is building. `beneath`, which finds the scan a filter belongs to, now walks through such a join, and the builder carries filters from above a join down its driving side into a list on the scan, next to the filter the scan's own join hands it. The side that builds does not see them, since its rows are a different table. A join keyed on more than one equality now offers a filter for each of them, and the first that resolves to a scan is armed.

## An exact bitmap instead of a filter

The filter hashes every row of the driving column and still lets about one in a hundred of the rows the join will drop through. When the build side's keys are integers that sit close together, one bit per value between the smallest and the largest is exact and costs a subtraction and a load instead of a hash. The orders of one year in q05 are 227 thousand keys over 6 million values, which is 26 bits a key, and the suppliers of one region are 2 thousand over 10 thousand.

The bitmap is built when the key is one of the four signed integer types, the range is at most 64 bits a key and no more than the filter's own budget of 32 MB. Past that the filter is smaller and a bit test that misses the cache is no cheaper than a filter probe. The bitmap takes the filter's place, it is checked before any filter because a row it drops is a row no filter has to hash, and it answers to the same count that turns a filter off when it keeps three quarters of the rows or more. The Parquet scan uses it too.

The first version tested each row with 128 bit arithmetic and a branch, which cost q03 about 6 ms. Doing the subtraction in 64 bits, letting a key under the base wrap round past the range so one compare covers both ends, and writing the row index whether it is kept or not took that back.

## What it did

CPU per query over ten runs in one process each, on a laptop busy with other work, so the numbers move by 5 percent between runs.

| query | DuckDB | before | through joins | and the bitmap |
|---|---|---|---|---|
| q02 | 26 | 32 | 23 | 21 |
| q05 | 79 | 182 | 130 | 116 |
| q07 | 87 | 112 | 93 | 91 |
| q18 | 248 | 296 | 196 | 199 |

q02 is now under DuckDB. q05 went from 2.3 times DuckDB to 1.5, and in its plan the lineitem scan hands up 203 thousand rows instead of 976 thousand. The other queries did not move, and every answer is the same bytes as before.

## What is left

q05 is still 1.5 times DuckDB, and q09, q10 and q21 did not move, because in those the rows that reach the probe are the rows that match. The remaining gap in them is in the cost of each probe and each gathered column, not in the number of rows, which is where the next note goes.

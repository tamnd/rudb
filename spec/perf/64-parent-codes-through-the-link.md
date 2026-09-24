# 64. Parent codes through the link

## The problem

With `graph_sections` on, q12 joins `lineitem` to `orders` through the stored link rather than a hash join, and at SF1 on one thread it retired 1.349 G instructions against 0.790 G for the hash join. The link join itself was 237 ms of CPU for 31 thousand rows in `EXPLAIN ANALYZE`, and most of that was flattening.

The one parent column q12 reads is `o_orderpriority`, which has five distinct values and is stored as codes into one dictionary for the whole table. A link join reads each parent column whole once, so that a child row can take its parent's value by row id, and that read flattened every part into strings. The column became a million and a half strings, and each chunk of the join handed up a gather over them. The aggregate above compares `o_orderpriority` with two literals, and the comparison flattened each gather again, so every chunk copied its strings out as well. The hash join keeps these codes since spec/perf/39, so the link join was doing string work the plan it replaces did not.

## The change

`Parent::read` keeps a part that is codes into the table's dictionary as it is, and when every part of the column shares one dictionary it lays the codes end to end with the nulls beside them, the same thing `side::coded` does for a hash join's gathered side. Parts that do not all share one are flattened and laid out as strings, as before.

`LinkJoin::gather` reads the codes for a coded parent column rather than handing up a gather. That is four bytes a row, and what comes out is a stable dictionary over the same values, which the comparison, grouping and sort kernels already read a code at a time. A row whose parent is missing or null is null. A column held any other way is still handed up as a gather.

## Results

Instructions per run at SF1 on one thread, on server3, main against this change, with the setting off and on:

| query | main off | main on | after off | after on |
|---|---|---|---|---|
| q12 | 790 M | 1349 M | 715 M | 717 M |
| q13 | 1032 M | 873 M | 1031 M | 876 M |
| q09 | 2035 M | 2272 M | 2002 M | 2259 M |

The main and after builds are on different commits of main, which is why the off column moves too. All 22 answers with the setting on match main with it off. There are new tests for a parent column of codes in one dictionary and in two, and for a coded gather with missing and null parents.

## What is left

With q12 level, the setting is a clear win on q13 alone. q09 is now worse with it on, because main has taken the hash join plan for q09 from 2.76 G to 2.0 G since the last measurement while the link plan moved less, so `graph_sections` stays off by default until q09's link plan is looked at again.

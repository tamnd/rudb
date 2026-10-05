# The order of a scan's filters, timed

Notes written on 5 October 2026, while finding out why the bitmap test of the joins was the largest cost in TPC-H q20.

## The question

In q20 `Domain::kept` was 16% of the query at SF1 on one thread, more than any other function. The lineitem scan has a pushed filter on `l_shipdate` that keeps about one row in seven, and a bitmap from the join to `partsupp` on `l_partkey` that keeps about one row in a hundred. Note 26 put every bitmap ahead of the pushed filter, because back then the pushed filter narrowed every column it kept and that unpacked the packed ones. Note 47 went further for a join that keeps few rows, and read the filter's columns only at the rows the bitmap kept.

So the scan tested the bitmap on all six million rows of lineitem, about four cycles a row, to save a date compare that note 106 had brought down to a fraction of a cycle a row. Neither order is right for every query. In q03 the date keeps half of lineitem and the orders bitmap keeps one row in a hundred, and there the bitmap over every row is still the cheaper order.

## What changed

When a scan has both a pushed filter and a bitmap, the first parts it reads take turns between the two orders. Each part is timed whole, from the read to the narrowed chunk, and after nine parts of each the scan keeps the order with the lower median for the rest of the table. The times are per row, so parts of different lengths compare.

The new order runs the pushed filter over the whole chunk, then asks each bitmap only about the rows the filter kept, and narrows the chunk once to what all of them kept. `Domain::retain` is the bitmap test over a list of rows. It reads a packed key a code at a time, which costs more a row than the block read over a whole chunk, and compacts the list without a branch on the answer. The Bloom filters still go last in both orders.

Working the costs out from the counts the scan keeps does not work. The pushed filter is only ever timed on the rows a bitmap has left, where its cost is mostly what the chunk around it costs, so a cheap compare over twenty rows looks dearer per row than a bitmap over two thousand. Timing both orders on real parts answers the question that is actually being asked.

## Measured

Single thread at SF1 on server2, the mean of eight warm runs, against main at #2465. The answers to all 22 queries are the same bytes as before.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q03 | 168 | 120 | 120 | 109 |
| q05 | 205 | 167 | 145 | 127 |
| q20 | 246 | 188 | 175 | 122 |

The other 19 queries run the same instructions as before within one million, and their cycles move within the noise of a machine with a load of twelve. On q20 that is 3.6 times fewer cycles than DuckDB's 435 million, up from 2.5.

q03 picked the new order too, though the date keeps half of lineitem. The bitmap there asked about half the rows still costs less than the narrowing the old order did between the bitmap and the filter.

## What this leaves

Note 110 suggested holding the sorted `l_orderkey` packed, so that the closed run walk of q18 reads fewer bytes. That was measured on a branch that held every stride page packed. q18 went from 175 to 193 million instructions with the same cycles, and the joins on `l_orderkey` each added 7 to 10 million, so it was not merged.

In q20 the scan of `part` with `p_name LIKE 'forest%'` runs twice, once for each copy of the subquery, and with the string compare it is now the largest cost left in the query.
